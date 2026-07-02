#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>

#include <zephyr/device.h>
#include <zephyr/drivers/gpio.h>
#include <zephyr/drivers/i2c.h>
#include <zephyr/drivers/spi.h>
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>

#define SPI_BUS_NODE DT_NODELABEL(arduino_spi)
#define SPI_CS_NODE  DT_NODELABEL(arduino_spi)
#define SPI_BUS_OPERATION                                                      \
    (SPI_OP_MODE_MASTER | SPI_WORD_SET(8) | SPI_TRANSFER_MSB)
#define SPI_BUS_FREQUENCY                                                      \
    DT_PROP(DT_NODELABEL(spi_probe_dev), spi_max_frequency)

#define I2C_BUS_NODE DT_NODELABEL(arduino_i2c)

static const struct device *const spi_bus = DEVICE_DT_GET(SPI_BUS_NODE);
static const struct spi_config    spi_cfg = {
       .frequency = SPI_BUS_FREQUENCY,
       .operation = SPI_BUS_OPERATION,
       .slave     = DT_REG_ADDR(DT_NODELABEL(spi_probe_dev)),
};
static const struct gpio_dt_spec spi_cs =
    GPIO_DT_SPEC_GET_BY_IDX(SPI_CS_NODE, cs_gpios, 0);

static const struct device *const i2c_bus = DEVICE_DT_GET(I2C_BUS_NODE);

#define RSEQ_INT1_NODE DT_NODELABEL(rseq_int1)

static const struct gpio_dt_spec rseq_int1 =
    GPIO_DT_SPEC_GET(RSEQ_INT1_NODE, gpios);
static struct gpio_callback rseq_int1_cb;
K_SEM_DEFINE(rseq_int1_sem, 0, 1);
static bool rseq_int1_ready;

// 声明 Rust 侧的回调函数
extern void rust_irq_int1_triggered(void);

static void rseq_int1_isr(const struct device *port, struct gpio_callback *cb,
                          uint32_t pins)
{
    (void)port;
    (void)cb;
    (void)pins;
    k_sem_give(&rseq_int1_sem);
    // 同时通知 Rust 侧设置待处理标志
    rust_irq_int1_triggered();
}

static void rseq_int1_sem_drain(void)
{
    while (k_sem_take(&rseq_int1_sem, K_NO_WAIT) == 0) {
    }
}

int rust_spi_bus_is_ready(void)
{
    return device_is_ready(spi_bus) ? 0 : -ENODEV;
}

int rust_spi_bus_transceive(const uint8_t *tx, size_t tx_len, uint8_t *rx,
                            size_t rx_len)
{
    const struct spi_buf tx_buf = {
        .buf = (void *)tx,
        .len = tx_len,
    };
    const struct spi_buf_set tx_set = {
        .buffers = &tx_buf,
        .count   = tx != NULL ? 1 : 0,
    };
    struct spi_buf rx_buf = {
        .buf = rx,
        .len = rx_len,
    };
    const struct spi_buf_set rx_set = {
        .buffers = &rx_buf,
        .count   = rx != NULL ? 1 : 0,
    };

    if (!device_is_ready(spi_bus)) {
        return -ENODEV;
    }

    return spi_transceive(spi_bus, &spi_cfg, tx != NULL ? &tx_set : NULL,
                          rx != NULL ? &rx_set : NULL);
}

int rust_spi_cs_init(void)
{
    if (!gpio_is_ready_dt(&spi_cs)) {
        return -ENODEV;
    }

    return gpio_pin_configure_dt(&spi_cs, GPIO_OUTPUT_INACTIVE);
}

int rust_spi_cs_set_low(void)
{
    return gpio_pin_set_raw(spi_cs.port, spi_cs.pin, 0);
}

int rust_spi_cs_set_high(void)
{
    return gpio_pin_set_raw(spi_cs.port, spi_cs.pin, 1);
}

int rust_i2c_bus_is_ready(void)
{
    return device_is_ready(i2c_bus) ? 0 : -ENODEV;
}

int rust_i2c_bus_read(uint16_t addr, uint8_t *data, size_t len)
{
    if (!device_is_ready(i2c_bus)) {
        return -ENODEV;
    }

    return i2c_read(i2c_bus, data, len, addr);
}

int rust_i2c_bus_write(uint16_t addr, const uint8_t *data, size_t len)
{
    if (!device_is_ready(i2c_bus)) {
        return -ENODEV;
    }

    return i2c_write(i2c_bus, data, len, addr);
}

int rust_i2c_bus_write_read(uint16_t addr, const uint8_t *write_data,
                            size_t write_len, uint8_t *read_data,
                            size_t read_len)
{
    if (!device_is_ready(i2c_bus)) {
        return -ENODEV;
    }

    return i2c_write_read(i2c_bus, addr, write_data, write_len, read_data,
                          read_len);
}

int rust_irq_init(void)
{
    int ret;

    if (rseq_int1_ready) {
        return 0;
    }

    if (!gpio_is_ready_dt(&rseq_int1)) {
        return -ENODEV;
    }

    ret = gpio_pin_configure_dt(&rseq_int1, GPIO_INPUT);
    if (ret != 0) {
        return ret;
    }

    gpio_init_callback(&rseq_int1_cb, rseq_int1_isr, BIT(rseq_int1.pin));
    ret = gpio_add_callback(rseq_int1.port, &rseq_int1_cb);
    if (ret != 0) {
        return ret;
    }

    ret = gpio_pin_interrupt_configure_dt(&rseq_int1, GPIO_INT_EDGE_TO_ACTIVE);
    if (ret != 0) {
        (void)gpio_remove_callback(rseq_int1.port, &rseq_int1_cb);
        return ret;
    }

    rseq_int1_sem_drain();
    rseq_int1_ready = true;
    return 0;
}

int rust_irq_wait(uint8_t pin, uint32_t timeout_ms)
{
    int ret;
    int level;
    int level_after;

    if (pin != 0U) {
        return -2;
    }

    ret = rust_irq_init();
    if (ret != 0) {
        return -3;
    }

    rseq_int1_sem_drain();
    level = gpio_pin_get_dt(&rseq_int1);
    if (level < 0) {
        return -3;
    }

    printk("rseq: irq wait pin=%u timeout=%u entry_level=%d\n",
           (unsigned int)pin, (unsigned int)timeout_ms, level);
    if (level > 0) {
        printk("rseq: irq wait pin=%u already high, waiting next edge\n",
               (unsigned int)pin);
    }

    ret = k_sem_take(&rseq_int1_sem, K_MSEC(timeout_ms));
    if (ret == 0) {
        level_after = gpio_pin_get_dt(&rseq_int1);
        printk("rseq: irq edge pin=%u level=%d\n", (unsigned int)pin,
               level_after);
        return 0;
    }

    printk("rseq: irq wait timeout pin=%u\n", (unsigned int)pin);
    return -1;
}
