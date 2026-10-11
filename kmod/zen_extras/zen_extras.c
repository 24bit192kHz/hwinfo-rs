// SPDX-License-Identifier: GPL-2.0
/*
 * Read-only Zen 2 / Zen 3 extras HWiNFO shows and k10temp doesn't, via the
 * kernel's locked amd_smn_read() (offsets from HWiNFO64 RE + ZenTimings):
 *   temp1 "IOD Hotspot" / temp2 "IOD Average": SMN 0x59824 + 4*i, i = 0..31,
 *       valid if bit 11, T = ((v >> 12) & 0xFFF) * 0.125 - 49 C
 *   throttle_{htc,prochot_cpu,prochot_ext}: SMN 0x59804 bits 4 / 3 / 2
 *   mem_*: UMC SMN 0x50200/0x50204/0x50208/0x50260 (channel A)
 * No SMN writes anywhere.
 */
#include <linux/hwmon.h>
#include <linux/hwmon-sysfs.h>
#include <linux/module.h>
#include <linux/platform_device.h>
#include <asm/amd/node.h>
#include <asm/cpu_device_id.h>
#include <asm/processor.h>

#define IOD_BASE	0x59824
#define THM_STATUS	0x59804
#define UMC_BASE	0x50000

static struct platform_device *pdev;
static struct device *hwdev;
static u32 iod_mask;

static int smn(u32 addr, u32 *v)
{
	return amd_smn_read(0, addr, v);
}

static int iod_temps(long *hot, long *avg)
{
	long max = LONG_MIN, sum = 0;
	int i, n = 0;
	u32 v;

	for (i = 0; i < 32; i++) {
		if (!(iod_mask & BIT(i)) || smn(IOD_BASE + 4 * i, &v) || !(v & BIT(11)))
			continue;
		/* millidegrees: ((v >> 12) & 0xfff) * 125 - 49000 */
		v = ((v >> 12) & 0xfff) * 125;
		max = max_t(long, max, (long)v - 49000);
		sum += (long)v - 49000;
		n++;
	}
	if (!n)
		return -ENODATA;
	*hot = max;
	*avg = sum / n;
	return 0;
}

static int z_read(struct device *dev, enum hwmon_sensor_types type, u32 attr,
		  int ch, long *val)
{
	long hot, avg;
	int ret = iod_temps(&hot, &avg);

	if (ret)
		return ret;
	*val = ch == 0 ? hot : avg;
	return 0;
}

static int z_read_string(struct device *dev, enum hwmon_sensor_types type,
			 u32 attr, int ch, const char **str)
{
	*str = ch == 0 ? "IOD Hotspot" : "IOD Average";
	return 0;
}

static umode_t z_visible(const void *d, enum hwmon_sensor_types type, u32 attr,
			 int ch)
{
	return 0444;
}

static const struct hwmon_channel_info * const z_info[] = {
	HWMON_CHANNEL_INFO(temp, HWMON_T_INPUT | HWMON_T_LABEL,
			   HWMON_T_INPUT | HWMON_T_LABEL),
	NULL
};

static const struct hwmon_ops z_ops = {
	.is_visible = z_visible,
	.read = z_read,
	.read_string = z_read_string,
};

static const struct hwmon_chip_info z_chip = { .ops = &z_ops, .info = z_info };

/* Extra read-only attributes, one show() over this table. */
struct field {
	const char *name;
	u32 addr;
	u8 shift, bits;
};

static const struct field fields[] = {
	{ "throttle_htc",	  THM_STATUS, 4, 1 },
	{ "throttle_prochot_cpu", THM_STATUS, 3, 1 },
	{ "throttle_prochot_ext", THM_STATUS, 2, 1 },
	{ "mem_ratio_x3",	  UMC_BASE + 0x200, 0, 7 },
	{ "mem_cmd2t",		  UMC_BASE + 0x200, 10, 1 },
	{ "mem_gdm",		  UMC_BASE + 0x200, 11, 1 },
	{ "mem_tcl",		  UMC_BASE + 0x204, 0, 6 },
	{ "mem_tras",		  UMC_BASE + 0x204, 8, 7 },
	{ "mem_trcdrd",		  UMC_BASE + 0x204, 16, 6 },
	{ "mem_trcdwr",		  UMC_BASE + 0x204, 24, 6 },
	{ "mem_trc",		  UMC_BASE + 0x208, 0, 8 },
	{ "mem_trp",		  UMC_BASE + 0x208, 16, 6 },
	{ "mem_trfc",		  UMC_BASE + 0x260, 0, 11 },
};

static ssize_t field_show(struct device *dev, struct device_attribute *da, char *buf)
{
	const struct field *f = &fields[to_sensor_dev_attr(da)->index];
	u32 v, addr = f->addr;

	/* ZenTimings: 0x50260 holds a default pattern when tRFC lives in 0x50264. */
	if (addr == UMC_BASE + 0x260) {
		if (smn(addr, &v))
			return -EIO;
		if (v == 0x21060138)
			addr += 4;
	}
	if (smn(addr, &v))
		return -EIO;
	return sysfs_emit(buf, "%u\n", (v >> f->shift) & ((1u << f->bits) - 1));
}

static struct sensor_device_attribute sda[ARRAY_SIZE(fields)];
static struct attribute *z_attrs[ARRAY_SIZE(fields) + 1];
static const struct attribute_group z_group = { .attrs = z_attrs };
static const struct attribute_group *z_groups[] = { &z_group, NULL };

/* Parts whose THM/UMC SMN layout matches (HWiNFO RE, ZenTimings): Zen 2
 * Matisse (17h/71h), Castle Peak (17h/31h); Zen 3 Vermeer (19h/21h), Milan
 * (19h/01h), Chagall (19h/08h). Also drives udev autoloading. */
static const struct x86_cpu_id z_cpus[] = {
	X86_MATCH_VENDOR_FAM_MODEL(AMD, 0x17, 0x31, NULL),
	X86_MATCH_VENDOR_FAM_MODEL(AMD, 0x17, 0x71, NULL),
	X86_MATCH_VENDOR_FAM_MODEL(AMD, 0x19, 0x01, NULL),
	X86_MATCH_VENDOR_FAM_MODEL(AMD, 0x19, 0x08, NULL),
	X86_MATCH_VENDOR_FAM_MODEL(AMD, 0x19, 0x21, NULL),
	{ }
};
MODULE_DEVICE_TABLE(x86cpu, z_cpus);

static int __init z_init(void)
{
	int i;
	u32 v;

	if (!x86_match_cpu(z_cpus))
		return -ENODEV;

	for (i = 0; i < 32; i++)
		if (!smn(IOD_BASE + 4 * i, &v) && (v & BIT(11)))
			iod_mask |= BIT(i);

	for (i = 0; i < ARRAY_SIZE(fields); i++) {
		sysfs_attr_init(&sda[i].dev_attr.attr);
		sda[i].dev_attr.attr.name = fields[i].name;
		sda[i].dev_attr.attr.mode = 0444;
		sda[i].dev_attr.show = field_show;
		sda[i].index = i;
		z_attrs[i] = &sda[i].dev_attr.attr;
	}

	pdev = platform_device_register_simple("zen_extras", -1, NULL, 0);
	if (IS_ERR(pdev))
		return PTR_ERR(pdev);
	hwdev = hwmon_device_register_with_info(&pdev->dev, "zen_extras", NULL,
						&z_chip, z_groups);
	if (IS_ERR(hwdev)) {
		platform_device_unregister(pdev);
		return PTR_ERR(hwdev);
	}
	pr_info("zen_extras: %d IOD sensors (mask %#x)\n", hweight32(iod_mask), iod_mask);
	return 0;
}

static void __exit z_exit(void)
{
	hwmon_device_unregister(hwdev);
	platform_device_unregister(pdev);
}

module_init(z_init);
module_exit(z_exit);
MODULE_DESCRIPTION("Read-only Zen 2/3 IOD temps, throttle flags and DRAM timings via SMN");
MODULE_LICENSE("GPL");
