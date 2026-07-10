/*
 * rseq MCU — Zephyr transport + UART FFI for Rust.
 *
 *  - rust_transport_init/read/write: the board-selected UART-like byte stream
 *    (USB CDC-ACM or hardware UART) used as the rseq-link Transport.
 *  - CONFIG_RSEQ_TRANSPORT_USB_CDC brings up Zephyr's "new" USB device stack
 *    before binding the CDC-ACM UART.
 *  - rust_uptime_us/kernel_delay_us/sleep_ms: timing for report metadata and
 *    the Bus::delay_us path.
 *  - rust_printk: raw console output from Rust.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

#include <stddef.h>
#include <stdint.h>
#include <errno.h>

#include <zephyr/kernel.h>
#include <zephyr/device.h>
#include <zephyr/drivers/uart.h>
#include <zephyr/devicetree.h>
#include <zephyr/irq.h>
#include <zephyr/sys/printk.h>

#ifdef CONFIG_RSEQ_TRANSPORT_USB_CDC
#include <zephyr/usb/usbd.h>
#include <zephyr/usb/usb_ch9.h>
#endif

/* ---- Optional USB device context + descriptors (self-contained CDC port) ---- */

#ifdef CONFIG_RSEQ_TRANSPORT_USB_CDC

#define USB_VID 0x0483
#define USB_PID 0x5740 /* ST + generic STM32 CDC; host binds cdc_acm/usbser */

USBD_DEVICE_DEFINE(udev,
		   DEVICE_DT_GET(DT_NODELABEL(zephyr_udc0)),
		   USB_VID, USB_PID);

USBD_DESC_LANG_DEFINE(udev_lang);
USBD_DESC_MANUFACTURER_DEFINE(udev_mfr, "rseq");
USBD_DESC_PRODUCT_DEFINE(udev_product, "rseq MCU CDC");
USBD_DESC_SERIAL_NUMBER_DEFINE(udev_sn); /* from hwinfo (chip UID) */
USBD_DESC_CONFIG_DEFINE(udev_cfg_desc, "CDC");

USBD_CONFIGURATION_DEFINE(udev_config,
			  USB_SCD_SELF_POWERED,
			  0, &udev_cfg_desc);

static void udev_msg_cb(struct usbd_context *const ctx, const struct usbd_msg *msg)
{
	if (usbd_can_detect_vbus(ctx)) {
		if (msg->type == USBD_MSG_VBUS_READY) {
			(void)usbd_enable(ctx);
		} else if (msg->type == USBD_MSG_VBUS_REMOVED) {
			(void)usbd_disable(ctx);
		}
	}
}

static int rust_usb_enable(void)
{
	int err;

	err  = usbd_add_descriptor(&udev, &udev_lang);
	err |= usbd_add_descriptor(&udev, &udev_mfr);
	err |= usbd_add_descriptor(&udev, &udev_product);
	err |= usbd_add_descriptor(&udev, &udev_sn);
	if (err) {
		printk("rseq: usbd_add_descriptor=%d\n", err);
		return -EIO;
	}

	err = usbd_add_configuration(&udev, USBD_SPEED_FS, &udev_config);
	if (err) {
		printk("rseq: usbd_add_configuration=%d\n", err);
		return err;
	}

	/* Registers the cdc_acm_uart0 instance as a class function. */
	err = usbd_register_all_classes(&udev, USBD_SPEED_FS, 1, NULL);
	if (err) {
		printk("rseq: usbd_register_all_classes=%d\n", err);
		return err;
	}

	(void)usbd_device_set_code_triple(&udev, USBD_SPEED_FS,
					  USB_BCC_MISCELLANEOUS, 0x02, 0x01);

	err = usbd_msg_register_cb(&udev, udev_msg_cb);
	if (err) {
		printk("rseq: usbd_msg_register_cb=%d\n", err);
		return err;
	}

	err = usbd_init(&udev);
	if (err) {
		printk("rseq: usbd_init=%d\n", err);
		return err;
	}

	if (!usbd_can_detect_vbus(&udev)) {
		err = usbd_enable(&udev);
		if (err) {
			printk("rseq: usbd_enable=%d\n", err);
			return err;
		}
	}

	printk("rseq: usb ok\n");
	return 0;
}

#endif /* CONFIG_RSEQ_TRANSPORT_USB_CDC */

/* ---- rseq transport UART-like device from /chosen rseq,transport ---- */

#if DT_HAS_CHOSEN(rseq_transport)
static const struct device *const transport_dev = DEVICE_DT_GET(DT_CHOSEN(rseq_transport));
#else
#error "missing /chosen rseq,transport in board overlay"
#endif

K_MSGQ_DEFINE(uart_rx_msgq, sizeof(uint8_t), 4096, 4);

#ifdef CONFIG_RSEQ_TRANSPORT_UART
#define UART_TX_RING_SIZE 8192U
#define UART_TX_IRQ_BUDGET 64U

static uint8_t uart_tx_ring[UART_TX_RING_SIZE];
static size_t uart_tx_head;
static size_t uart_tx_tail;
static size_t uart_tx_count;
static uint32_t uart_tx_dropped_frames;

static void uart_tx_reset(void)
{
	unsigned int key = irq_lock();

	uart_tx_head = 0U;
	uart_tx_tail = 0U;
	uart_tx_count = 0U;
	uart_tx_dropped_frames = 0U;
	irq_unlock(key);

	uart_irq_tx_disable(transport_dev);
}

static int uart_tx_enqueue(const uint8_t *data, size_t len)
{
	if (len == 0U) {
		return 0;
	}

	if (len > UART_TX_RING_SIZE) {
		return -EMSGSIZE;
	}

	unsigned int key = irq_lock();
	size_t free = UART_TX_RING_SIZE - uart_tx_count;

	if (free < len) {
		uart_tx_dropped_frames++;
		irq_unlock(key);
		return -ENOSPC;
	}

	for (size_t i = 0U; i < len; i++) {
		uart_tx_ring[uart_tx_head] = data[i];
		uart_tx_head = (uart_tx_head + 1U) % UART_TX_RING_SIZE;
	}
	uart_tx_count += len;
	irq_unlock(key);

	uart_irq_tx_enable(transport_dev);
	return 0;
}

static void uart_tx_service(const struct device *dev)
{
	for (uint32_t budget = 0U; budget < UART_TX_IRQ_BUDGET; budget++) {
		if (uart_tx_count == 0U) {
			uart_irq_tx_disable(dev);
			return;
		}

		uint8_t byte = uart_tx_ring[uart_tx_tail];
		int written = uart_fifo_fill(dev, &byte, 1);
		if (written <= 0) {
			return;
		}

		uart_tx_tail = (uart_tx_tail + 1U) % UART_TX_RING_SIZE;
		uart_tx_count--;
	}
}
#else
K_MUTEX_DEFINE(uart_tx_mutex);
#endif

K_SEM_DEFINE(rseq_event_sem, 0, 1);

void rust_event_notify(void)
{
	k_sem_give(&rseq_event_sem);
}

int rust_event_wait(uint32_t timeout_ms)
{
	return k_sem_take(&rseq_event_sem,
			  timeout_ms == 0U ? K_FOREVER : K_MSEC(timeout_ms));
}

static void uart_irq_cb(const struct device *dev, void *user_data)
{
	ARG_UNUSED(user_data);

	uart_irq_update(dev);

	if (uart_irq_rx_ready(dev)) {
		uint8_t buf[32];

		while (1) {
			int n = uart_fifo_read(dev, buf, sizeof(buf));
			if (n <= 0) {
				break;
			}

			for (int i = 0; i < n; i++) {
				(void)k_msgq_put(&uart_rx_msgq, &buf[i], K_NO_WAIT);
			}
			rust_event_notify();
		}
	}

#ifdef CONFIG_RSEQ_TRANSPORT_UART
	if (uart_irq_tx_ready(dev)) {
		uart_tx_service(dev);
	}
#endif
}

int rust_transport_init(void)
{
#ifdef CONFIG_RSEQ_TRANSPORT_USB_CDC
	int usb_ret = rust_usb_enable();
	if (usb_ret != 0) {
		return usb_ret;
	}
#endif

	if (!device_is_ready(transport_dev)) {
		printk("rseq: transport device not ready\n");
		return -ENODEV;
	}

	printk("rseq: transport=%s\n", transport_dev->name);

#ifdef CONFIG_RSEQ_TRANSPORT_UART
	struct uart_config cfg = {
		.baudrate = DT_PROP(DT_CHOSEN(rseq_transport), current_speed),
		.parity = UART_CFG_PARITY_NONE,
		.stop_bits = UART_CFG_STOP_BITS_1,
		.data_bits = UART_CFG_DATA_BITS_8,
		.flow_ctrl = UART_CFG_FLOW_CTRL_NONE,
	};

	int cfg_ret = uart_configure(transport_dev, &cfg);
	if (cfg_ret != 0 && cfg_ret != -ENOSYS) {
		printk("rseq: uart_configure=%d\n", cfg_ret);
		return cfg_ret;
	}
#endif

	k_msgq_purge(&uart_rx_msgq);

	int ret = uart_irq_callback_user_data_set(transport_dev, uart_irq_cb, NULL);
	if (ret) {
		printk("rseq: uart_irq_callback_set=%d\n", ret);
		return ret;
	}

#ifdef CONFIG_RSEQ_TRANSPORT_UART
	uart_tx_reset();
#endif

	uart_irq_rx_enable(transport_dev);
	return 0;
}

/* Non-blocking read: always returns immediately (0 if no data).
 * Used for IRQ polling mode. */
int rust_transport_read(uint8_t *buf, size_t len)
{
	if (len == 0 || buf == NULL) {
		return -EINVAL;
	}

	size_t count = 0;
	while (count < len) {
		if (k_msgq_get(&uart_rx_msgq, &buf[count], K_NO_WAIT) != 0) {
			break;
		}
		count++;
	}

#ifdef CONFIG_RSEQ_TRANSPORT_UART
	while (count < len) {
		if (uart_poll_in(transport_dev, &buf[count]) != 0) {
			break;
		}
		count++;
	}
#endif

	return (int)count;
}

int rust_transport_write(const uint8_t *data, size_t len)
{
	if (len != 0 && data == NULL) {
		return -EINVAL;
	}

	if (!device_is_ready(transport_dev)) {
		return -ENODEV;
	}

#ifdef CONFIG_RSEQ_TRANSPORT_UART
	return uart_tx_enqueue(data, len);
#else
#if defined(CONFIG_RSEQ_TRANSPORT_USB_CDC) && defined(CONFIG_UART_LINE_CTRL)
	uint32_t dtr = 0U;
	int ctrl_ret = uart_line_ctrl_get(transport_dev, UART_LINE_CTRL_DTR, &dtr);
	if (ctrl_ret == 0 && dtr == 0U) {
		return -ENOTCONN;
	}
#endif

	k_mutex_lock(&uart_tx_mutex, K_FOREVER);
	for (size_t i = 0; i < len; i++) {
#if defined(CONFIG_RSEQ_TRANSPORT_USB_CDC) && defined(CONFIG_UART_LINE_CTRL)
		if ((i & 0x3fU) == 0U) {
			dtr = 0U;
			ctrl_ret = uart_line_ctrl_get(transport_dev, UART_LINE_CTRL_DTR, &dtr);
			if (ctrl_ret == 0 && dtr == 0U) {
				k_mutex_unlock(&uart_tx_mutex);
				return -ENOTCONN;
			}
		}
#endif
		uart_poll_out(transport_dev, data[i]);
	}
	k_mutex_unlock(&uart_tx_mutex);
	return 0;
#endif
}

void rust_kernel_sleep_ms(uint32_t ms)
{
	k_sleep(K_MSEC(ms));
}

uint64_t rust_uptime_us(void)
{
	return k_cyc_to_us_floor64(k_cycle_get_64());
}

void rust_kernel_delay_us(uint32_t us)
{
	if (us == 0) {
		return;
	}

	uint32_t ms = us / 1000U;
	uint32_t rem = us % 1000U;

	if (ms != 0U) {
		k_sleep(K_MSEC(ms));
	}

	if (rem != 0U) {
		k_busy_wait(rem);
	}
}

/* Raw console output for Rust; backend is selected by the board profile. */
void rust_printk(const char *s, size_t len)
{
	printk("%.*s", (int)len, s);
}
