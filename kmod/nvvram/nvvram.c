// SPDX-License-Identifier: GPL-2.0
/*
 * hwmon for the GDDR6X junction temperature of NVIDIA Ampere/Ada cards.
 *
 * The driver does not report memory temperature on GeForce boards through
 * NVML, but the GPU mirrors it in BAR0 at 0xE2A8 (same register the
 * userspace gddr6 tool reads through /dev/mem): temp = (raw & 0xfff) / 32.
 * This maps just that page read-only alongside the nvidia driver, so no
 * iomem=relaxed is needed. Reads are skipped while the GPU is not in D0
 * (runtime suspended) so we never wake it.
 */
#include <linux/hwmon.h>
#include <linux/io.h>
#include <linux/module.h>
#include <linux/pci.h>
#include <linux/pm_runtime.h>

#define VRAM_TEMP_REG	0xE2A8
#define THERM_PAGE	0x20000

static const u32 hotspot_regs[] = {
	0x0203B8, 0x0203BC, 0x02046C, 0x020470, 0x020478,
	0x0204B8, 0x0204BC, 0x0204C0, 0x0204C4,
};

/* Ampere / Ada boards whose VRAM temp is at 0xE2A8 (list from the gddr6 tool). */
static const struct pci_device_id nvvram_ids[] = {
	{ PCI_VDEVICE(NVIDIA, 0x2203) },	/* GA102 RTX 3090 Ti */
	{ PCI_VDEVICE(NVIDIA, 0x2204) },	/* GA102 RTX 3090 */
	{ PCI_VDEVICE(NVIDIA, 0x2206) },	/* GA102 RTX 3080 */
	{ PCI_VDEVICE(NVIDIA, 0x2208) },	/* GA102 RTX 3080 Ti */
	{ PCI_VDEVICE(NVIDIA, 0x2216) },	/* GA102 RTX 3080 LHR */
	{ PCI_VDEVICE(NVIDIA, 0x2231) },	/* GA102 RTX A5000 */
	{ PCI_VDEVICE(NVIDIA, 0x2232) },	/* GA102 RTX A4500 */
	{ PCI_VDEVICE(NVIDIA, 0x2236) },	/* GA102 A10 */
	{ PCI_VDEVICE(NVIDIA, 0x24b0) },	/* GA104 RTX A4000 */
	{ PCI_VDEVICE(NVIDIA, 0x2531) },	/* GA106 RTX A2000 */
	{ PCI_VDEVICE(NVIDIA, 0x2571) },	/* GA106 RTX A2000 */
	{ PCI_VDEVICE(NVIDIA, 0x2684) },	/* AD102 RTX 4090 */
	{ PCI_VDEVICE(NVIDIA, 0x2685) },	/* AD102 RTX 4090 D */
	{ PCI_VDEVICE(NVIDIA, 0x26b1) },	/* AD102 RTX 6000 Ada Generation */
	{ PCI_VDEVICE(NVIDIA, 0x26b9) },	/* AD102 L40S */
	{ PCI_VDEVICE(NVIDIA, 0x2702) },	/* AD103 RTX 4080 Super */
	{ PCI_VDEVICE(NVIDIA, 0x2704) },	/* AD103 RTX 4080 */
	{ PCI_VDEVICE(NVIDIA, 0x2705) },	/* AD103 RTX 4070 Ti Super */
	{ PCI_VDEVICE(NVIDIA, 0x2782) },	/* AD104 RTX 4070 Ti */
	{ PCI_VDEVICE(NVIDIA, 0x2783) },	/* AD104 RTX 4070 Super */
	{ PCI_VDEVICE(NVIDIA, 0x2786) },	/* AD104 RTX 4070 */
	{ PCI_VDEVICE(NVIDIA, 0x27b8) },	/* AD104 L4 */
	{ PCI_VDEVICE(NVIDIA, 0x2860) },	/* AD106 RTX 4070 Max-Q / Mobile */
	{ PCI_VDEVICE(NVIDIA, 0x28a0) },	/* AD107 RTX 4060 Laptop */
	{ PCI_VDEVICE(NVIDIA, 0x28e0) },	/* AD107 RTX 4060 Max-Q / Mobile */
	{ }
};
/* Lets udev autoload the module only on listed GPUs (it does not bind them). */
MODULE_DEVICE_TABLE(pci, nvvram_ids);

struct nvvram {
	struct pci_dev *pdev;
	void __iomem *page;
	void __iomem *therm;
	struct device *hwmon;
};

static struct nvvram cards[4];
static int ncards;

static int nvvram_read(struct device *dev, enum hwmon_sensor_types type,
		       u32 attr, int channel, long *val)
{
	struct nvvram *c = dev_get_drvdata(dev);
	long best = LONG_MIN;
	u32 raw;
	int ret, i;

	if (type != hwmon_temp || attr != hwmon_temp_input)
		return -EOPNOTSUPP;

	ret = pm_runtime_get_if_active(&c->pdev->dev);
	if (ret == 0)
		return -ENODATA;	/* asleep: don't wake it for a sensor */
	if (c->pdev->current_state != PCI_D0) {
		if (ret > 0)
			pm_runtime_put(&c->pdev->dev);
		return -ENODATA;
	}
	if (channel == 1) {
		for (i = 0; i < ARRAY_SIZE(hotspot_regs); i++) {
			u32 w = readl(c->therm + (hotspot_regs[i] - THERM_PAGE));
			u8 b1 = (w >> 8) & 0xff;

			if ((w >> 16) != 0x4000 || b1 == 0 || b1 == 0xff)
				continue;
			best = max_t(long, best, (long)(s16)(w & 0xffff) * 1000 / 256);
		}
		if (ret > 0)
			pm_runtime_put(&c->pdev->dev);
		if (best == LONG_MIN)
			return -ENODATA;
		*val = best;
		return 0;
	}
	raw = readl(c->page + (VRAM_TEMP_REG & ~PAGE_MASK));
	if (ret > 0)
		pm_runtime_put(&c->pdev->dev);

	if (raw == 0xffffffff || !(raw & 0xfff))
		return -ENODATA;
	*val = (raw & 0xfff) * 1000L / 32;
	return 0;
}

static int nvvram_read_string(struct device *dev,
			      enum hwmon_sensor_types type, u32 attr,
			      int channel, const char **str)
{
	*str = channel == 1 ? "GPU Hot Spot" : "VRAM Junction";
	return 0;
}

static umode_t nvvram_visible(const void *data, enum hwmon_sensor_types type,
			      u32 attr, int channel)
{
	return 0444;
}

static const struct hwmon_channel_info * const nvvram_info[] = {
	HWMON_CHANNEL_INFO(temp, HWMON_T_INPUT | HWMON_T_LABEL,
			   HWMON_T_INPUT | HWMON_T_LABEL),
	NULL
};

static const struct hwmon_ops nvvram_ops = {
	.is_visible	= nvvram_visible,
	.read		= nvvram_read,
	.read_string	= nvvram_read_string,
};

static const struct hwmon_chip_info nvvram_chip = {
	.ops	= &nvvram_ops,
	.info	= nvvram_info,
};

static void nvvram_cleanup(void)
{
	while (ncards--) {
		struct nvvram *c = &cards[ncards];

		hwmon_device_unregister(c->hwmon);
		iounmap(c->therm);
		iounmap(c->page);
		pci_dev_put(c->pdev);
	}
}

static int __init nvvram_init(void)
{
	const struct pci_device_id *id;
	struct pci_dev *pdev = NULL;

	while ((pdev = pci_get_device(PCI_VENDOR_ID_NVIDIA, PCI_ANY_ID, pdev))) {
		struct nvvram *c;

		if (ncards == ARRAY_SIZE(cards))
			break;
		id = pci_match_id(nvvram_ids, pdev);
		if (!id || !(pci_resource_flags(pdev, 0) & IORESOURCE_MEM) ||
		    pci_resource_len(pdev, 0) <= VRAM_TEMP_REG)
			continue;

		c = &cards[ncards];
		c->page = ioremap(pci_resource_start(pdev, 0) +
				  (VRAM_TEMP_REG & PAGE_MASK), PAGE_SIZE);
		if (!c->page)
			continue;
		c->therm = ioremap(pci_resource_start(pdev, 0) + THERM_PAGE, PAGE_SIZE);
		if (!c->therm) {
			iounmap(c->page);
			continue;
		}
		c->pdev = pci_dev_get(pdev);
		c->hwmon = hwmon_device_register_with_info(&pdev->dev, "nvvram",
							   c, &nvvram_chip,
							   NULL);
		if (IS_ERR(c->hwmon)) {
			iounmap(c->therm);
			iounmap(c->page);
			pci_dev_put(c->pdev);
			continue;
		}
		ncards++;
	}
	return ncards ? 0 : -ENODEV;
}

static void __exit nvvram_exit(void)
{
	nvvram_cleanup();
}

module_init(nvvram_init);
module_exit(nvvram_exit);
MODULE_DESCRIPTION("GDDR6/6X VRAM and hot-spot temperature hwmon for NVIDIA Ampere/Ada");
MODULE_LICENSE("GPL");
