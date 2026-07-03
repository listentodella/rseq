/*
 * rseq MCU — USB CDC + UART FFI for Rust.
 *
 *  - rust_usb_enable(): bring up Zephyr's "new" USB device stack (one CDC-ACM
 *    port) so the host sees "rseq F429ZI CDC". Each step is printk'd for
 *    bring-up diagnostics (logs go to USART3 / ST-Link VCP).
 *  - rust_uart_init/read/write: the CDC-ACM UART (RX irq→K_MSGQ, blocking;
 *    TX uart_poll_out under a mutex) used as the rseq-link Transport.
 *  - rust_uptime_us/kernel_delay_us/sleep_ms: timing for report metadata and
 *    the Bus::delay_us path.
 *  - rust_printk: raw console output from Rust (bypasses the log backend).
 *
 * SPDX-License-Identifier: Apache-2.0
 */

#include <stddef.h>
#include <stdint.h>
#include <errno.h>

#include <zephyr/kernel.h>
#include <zephyr/device.h>
#include <zephyr/drivers/uart.h>
#include <zephyr/sys/printk.h>

#include <zephyr/usb/usbd.h>
#include <zephyr/usb/usb_ch9.h>

/* ---- USB device context + descriptors (self-contained, 1 CDC port) ---- */

#define USB_VID 0x0483
#define USB_PID 0x5740 /* ST + generic STM32 CDC; host binds cdc_acm/usbser */

USBD_DEVICE_DEFINE(udev,
		   DEVICE_DT_GET(DT_NODELABEL(zephyr_udc0)),
		   USB_VID, USB_PID);

USBD_DESC_LANG_DEFINE(udev_lang);
USBD_DESC_MANUFACTURER_DEFINE(udev_mfr, "rseq");
USBD_DESC_PRODUCT_DEFINE(udev_product, "rseq F429ZI CDC");
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

int rust_usb_enable(void)
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

/* ---- CDC-ACM UART (cdc_acm_uart0 from app.overlay) ---- */

#if DT_NODE_EXISTS(DT_NODELABEL(cdc_acm_uart0))
static const struct device *const serial_dev = DEVICE_DT_GET(DT_NODELABEL(cdc_acm_uart0));
#else
#error "cdc_acm_uart0 not found in devicetree (check app.overlay)"
#endif

K_MSGQ_DEFINE(uart_rx_msgq, sizeof(uint8_t), 1024, 4);
K_MUTEX_DEFINE(uart_tx_mutex);
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
	if (!uart_irq_rx_ready(dev)) {
		return;
	}

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

int rust_uart_init(void)
{
	if (!device_is_ready(serial_dev)) {
		printk("rseq: cdc_acm_uart0 not ready\n");
		return -ENODEV;
	}

	k_msgq_purge(&uart_rx_msgq);

	int ret = uart_irq_callback_user_data_set(serial_dev, uart_irq_cb, NULL);
	if (ret) {
		printk("rseq: uart_irq_callback_set=%d\n", ret);
		return ret;
	}

	uart_irq_rx_enable(serial_dev);
	return 0;
}

/* Non-blocking read: always returns immediately (0 if no data).
 * Used for IRQ polling mode. */
int rust_uart_read(uint8_t *buf, size_t len)
{
	if (len == 0 || buf == NULL) {
		return -EINVAL;
	}

	/* Non-blocking: return immediately if no data */
	int ret = k_msgq_get(&uart_rx_msgq, &buf[0], K_NO_WAIT);
	if (ret) {
		/* No data available, return 0 (not an error) */
		return 0;
	}

	size_t count = 1;
	while (count < len) {
		if (k_msgq_get(&uart_rx_msgq, &buf[count], K_NO_WAIT) != 0) {
			break;
		}
		count++;
	}

	return (int)count;
}

int rust_uart_write(const uint8_t *data, size_t len)
{
	if (len != 0 && data == NULL) {
		return -EINVAL;
	}

	if (!device_is_ready(serial_dev)) {
		return -ENODEV;
	}

	k_mutex_lock(&uart_tx_mutex, K_FOREVER);
	for (size_t i = 0; i < len; i++) {
		uart_poll_out(serial_dev, data[i]);
	}
	k_mutex_unlock(&uart_tx_mutex);
	return 0;
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

/* Raw console output for Rust (bypasses the log backend; goes to USART3). */
void rust_printk(const char *s, size_t len)
{
	printk("%.*s", (int)len, s);
}
