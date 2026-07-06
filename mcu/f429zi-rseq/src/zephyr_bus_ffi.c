#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>
#include <errno.h>

#include <zephyr/device.h>
#include <zephyr/drivers/gpio.h>
#include <zephyr/drivers/i2c.h>
#include <zephyr/drivers/spi.h>
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>

#define RSEQ_SPI_BUS_NODE   DT_ALIAS(rseq_spi)
#define RSEQ_I2C_BUS_NODE   DT_ALIAS(rseq_i2c)
#define RSEQ_INT1_NODE      DT_ALIAS(rseq_int1)

#define RSEQ_HAS_SPI                                                           \
    (DT_NODE_EXISTS(RSEQ_SPI_BUS_NODE) &&                                      \
     DT_NODE_HAS_PROP(RSEQ_SPI_BUS_NODE, cs_gpios))
#define RSEQ_HAS_I2C DT_NODE_EXISTS(RSEQ_I2C_BUS_NODE)
#define RSEQ_HAS_INT1 DT_NODE_EXISTS(RSEQ_INT1_NODE)

#define SPI_BUS_FREQUENCY 8000000U
#define SPI_BUS_SLAVE     0U

#define SPI_BUS_OPERATION                                                      \
    (SPI_OP_MODE_MASTER | SPI_WORD_SET(8) | SPI_TRANSFER_MSB)

#if RSEQ_HAS_SPI
static const struct device *const spi_bus = DEVICE_DT_GET(RSEQ_SPI_BUS_NODE);
static const struct spi_config    spi_cfg = {
       .frequency = SPI_BUS_FREQUENCY,
       .operation = SPI_BUS_OPERATION,
       .slave     = SPI_BUS_SLAVE,
};
static const struct gpio_dt_spec spi_cs =
    GPIO_DT_SPEC_GET_BY_IDX(RSEQ_SPI_BUS_NODE, cs_gpios, 0);
#endif

#if RSEQ_HAS_I2C
static const struct device *const i2c_bus = DEVICE_DT_GET(RSEQ_I2C_BUS_NODE);
#endif

#if RSEQ_HAS_INT1
static const struct gpio_dt_spec rseq_int1 =
    GPIO_DT_SPEC_GET(RSEQ_INT1_NODE, gpios);
static struct gpio_callback rseq_int1_cb;
K_SEM_DEFINE(rseq_int1_sem, 0, 1);
static bool rseq_int1_ready;
#endif

// 声明 Rust 侧的回调函数
extern void rust_irq_int1_triggered(void);
extern void rust_event_notify(void);

#if RSEQ_HAS_INT1
static void rseq_int1_isr(const struct device *port, struct gpio_callback *cb,
                          uint32_t pins)
{
    (void)port;
    (void)cb;
    (void)pins;
    k_sem_give(&rseq_int1_sem);
    // 同时通知 Rust 侧设置待处理标志
    rust_irq_int1_triggered();
    rust_event_notify();
}

static void rseq_int1_sem_drain(void)
{
    while (k_sem_take(&rseq_int1_sem, K_NO_WAIT) == 0) {
    }
}
#endif

int rust_spi_bus_is_ready(void)
{
#if RSEQ_HAS_SPI
    return device_is_ready(spi_bus) ? 0 : -ENODEV;
#else
    return -ENOTSUP;
#endif
}

int rust_spi_bus_transceive(const uint8_t *tx, size_t tx_len, uint8_t *rx,
                            size_t rx_len)
{
#if RSEQ_HAS_SPI
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
#else
    (void)tx;
    (void)tx_len;
    (void)rx;
    (void)rx_len;
    return -ENOTSUP;
#endif
}

int rust_spi_cs_init(void)
{
#if RSEQ_HAS_SPI
    if (!gpio_is_ready_dt(&spi_cs)) {
        return -ENODEV;
    }

    return gpio_pin_configure_dt(&spi_cs, GPIO_OUTPUT_INACTIVE);
#else
    return -ENOTSUP;
#endif
}

int rust_spi_cs_set_low(void)
{
#if RSEQ_HAS_SPI
    return gpio_pin_set_raw(spi_cs.port, spi_cs.pin, 0);
#else
    return -ENOTSUP;
#endif
}

int rust_spi_cs_set_high(void)
{
#if RSEQ_HAS_SPI
    return gpio_pin_set_raw(spi_cs.port, spi_cs.pin, 1);
#else
    return -ENOTSUP;
#endif
}

int rust_i2c_bus_is_ready(void)
{
#if RSEQ_HAS_I2C
    return device_is_ready(i2c_bus) ? 0 : -ENODEV;
#else
    return -ENOTSUP;
#endif
}

int rust_i2c_bus_read(uint16_t addr, uint8_t *data, size_t len)
{
#if RSEQ_HAS_I2C
    if (!device_is_ready(i2c_bus)) {
        return -ENODEV;
    }

    return i2c_read(i2c_bus, data, len, addr);
#else
    (void)addr;
    (void)data;
    (void)len;
    return -ENOTSUP;
#endif
}

int rust_i2c_bus_write(uint16_t addr, const uint8_t *data, size_t len)
{
#if RSEQ_HAS_I2C
    if (!device_is_ready(i2c_bus)) {
        return -ENODEV;
    }

    return i2c_write(i2c_bus, data, len, addr);
#else
    (void)addr;
    (void)data;
    (void)len;
    return -ENOTSUP;
#endif
}

int rust_i2c_bus_write_read(uint16_t addr, const uint8_t *write_data,
                            size_t write_len, uint8_t *read_data,
                            size_t read_len)
{
#if RSEQ_HAS_I2C
    if (!device_is_ready(i2c_bus)) {
        return -ENODEV;
    }

    return i2c_write_read(i2c_bus, addr, write_data, write_len, read_data,
                          read_len);
#else
    (void)addr;
    (void)write_data;
    (void)write_len;
    (void)read_data;
    (void)read_len;
    return -ENOTSUP;
#endif
}

int rust_i3c_bus_is_ready(void)
{
    return -ENOTSUP;
}

int rust_i3c_bus_write_read(uint16_t addr, const uint8_t *write_data,
                            size_t write_len, uint8_t *read_data,
                            size_t read_len)
{
    (void)addr;
    (void)write_data;
    (void)write_len;
    (void)read_data;
    (void)read_len;
    return -ENOTSUP;
}

int rust_i3c_bus_write(uint16_t addr, const uint8_t *data, size_t len)
{
    (void)addr;
    (void)data;
    (void)len;
    return -ENOTSUP;
}

int rust_irq_init(void)
{
#if RSEQ_HAS_INT1
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
#else
    return -ENOTSUP;
#endif
}

int rust_irq_wait(uint8_t pin, uint32_t timeout_ms)
{
#if RSEQ_HAS_INT1
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
#else
    (void)pin;
    (void)timeout_ms;
    return -ENOTSUP;
#endif
}

int rust_irq_level(uint8_t pin)
{
#if RSEQ_HAS_INT1
    int ret;

    if (pin != 0U) {
        return -ENOTSUP;
    }

    ret = rust_irq_init();
    if (ret != 0) {
        return ret;
    }

    return gpio_pin_get_dt(&rseq_int1);
#else
    (void)pin;
    return -ENOTSUP;
#endif
}
