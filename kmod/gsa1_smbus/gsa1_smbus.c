// SPDX-License-Identifier: GPL-2.0
/*
 * SMBus adapter for the DDR4 thermal sensors (JC-42.4 TSOD at
 * 0x18-0x1B) on the Gigabyte B550 AORUS ELITE V2.
 *
 * The board's ACPI reserves the FCH SMBus at 0xB00 (GSA1.SMBI), so
 * i2c_piix4 gets no bus without acpi_enforce_resources=lax. Instead of
 * touching the controller directly, this calls the firmware's own AML
 * helpers, which take the firmware mutex SME0:
 *   \GSA1.SMM3(2, addr8, reg) -> SMBus read byte  (0xFFFF on error)
 *   \GSA1.SMM5(2, addr8, reg) -> SMBus read word  (0xFFFFFFFF on error)
 *   \GSA1.SMM4(2, addr8, reg, word) -> SMBus write word (1 = ok)
 *   \GSA1.SMM2(2, addr8, reg, byte) -> SMBus write byte
 * Only 0x18-0x1B are reachable. Writes are limited to the TSOD config and
 * limit registers (0x01-0x04), which jc42 uses for shutdown-on-suspend and
 * the alarm thresholds; everything else is refused.
 *
 * v1.1: hwmon "raa229004" for the CPU VRM (PMBus 0x60), read the way HWiNFO
 * does (RE in analysis/RE-REPORT.md): PAGE 0 = VDDCR_CPU,
 * PAGE 1 = VDDCR_SOC, word reads of 0x8D/0x8E/0x8B/0x88/0x8C/0x89/0x96/0x97
 * in Renesas fixed LSBs. The only VRM write is PAGE (0x00) = 0 or 1, and
 * PAGE is always put back to 0. The VRM is not exposed on the i2c adapter.
 */
#include <linux/acpi.h>
#include <linux/dmi.h>
#include <linux/hwmon.h>
#include <linux/jiffies.h>
#include <linux/mutex.h>
#include <linux/i2c.h>
#include <linux/module.h>

#define TSOD_FIRST	0x18
#define TSOD_LAST	0x1b
#define TSOD_REG_CONFIG	0x01
#define TSOD_REG_CRIT	0x04

#define VRM_ADDR	0x60

static acpi_handle gsa1;
static struct i2c_adapter adap;
static struct i2c_client *clients[TSOD_LAST - TSOD_FIRST + 1];

static int gsa1_call4(const char *method, u16 addr, u8 reg, u16 word,
		      int nargs, u64 *out)
{
	union acpi_object args[4] = {
		{ .integer = { ACPI_TYPE_INTEGER, 2 } },	/* port 0xB00 */
		{ .integer = { ACPI_TYPE_INTEGER, addr << 1 } },
		{ .integer = { ACPI_TYPE_INTEGER, reg } },
		{ .integer = { ACPI_TYPE_INTEGER, word } },
	};
	struct acpi_object_list list = { nargs, args };
	unsigned long long val;
	acpi_status st;

	st = acpi_evaluate_integer(gsa1, (acpi_string)method, &list, &val);
	if (ACPI_FAILURE(st))
		return -EIO;
	*out = val;
	return 0;
}

static int gsa1_call(const char *method, u16 addr, u8 reg, u64 *out)
{
	return gsa1_call4(method, addr, reg, 0, 3, out);
}

/* ---- RAA229004 VRM ---- */

enum { V_T1, V_T2, V_VIN, V_IIN, V_VOUT, V_IOUT, V_POUT, V_PIN, V_N };
static const u8 vrm_cmd[V_N] = { 0x8D, 0x8E, 0x88, 0x89, 0x8B, 0x8C, 0x96, 0x97 };

static struct device *vrm_hwmon;
static DEFINE_MUTEX(vrm_lock);
static unsigned long vrm_stamp;
static u16 vrm_raw[2][V_N];
static bool vrm_ok[2][V_N];

static int vrm_page(u8 page)
{
	u64 v;
	int ret = gsa1_call4("SMM2", VRM_ADDR, 0x00, page, 4, &v);

	return ret ? ret : (v == 1 ? 0 : -EIO);
}

/* One HWiNFO-style poll of both pages, at most once a second. */
static void vrm_update(void)
{
	int p, i;
	u64 v;

	if (vrm_stamp && time_before(jiffies, vrm_stamp + HZ))
		return;
	for (p = 0; p < 2; p++) {
		if (vrm_page(p)) {
			memset(vrm_ok[p], 0, sizeof(vrm_ok[p]));
			continue;
		}
		for (i = 0; i < V_N; i++) {
			vrm_ok[p][i] = !gsa1_call("SMM5", VRM_ADDR, vrm_cmd[i], &v) && v <= 0xffff;
			vrm_raw[p][i] = vrm_ok[p][i] ? v : 0;
		}
	}
	vrm_page(0);
	vrm_stamp = jiffies;
}

/* channel maps: temp 0/1 = loop1/2; in 0 = VIN, 1/2 = VOUT cpu/soc;
 * curr 0/3 = IIN cpu/soc, 1/2 = IOUT cpu/soc; power 0/3 = PIN cpu/soc,
 * 1/2 = POUT cpu/soc (IIN/PIN are per page on the RAA229004) */
static int vrm_read(struct device *dev, enum hwmon_sensor_types type, u32 attr,
		    int ch, long *val)
{
	int page = (ch == 2 || ch == 3) ? 1 : 0, idx;
	long raw;

	switch (type) {
	case hwmon_temp: idx = ch ? V_T2 : V_T1; break;
	case hwmon_in: idx = ch ? V_VOUT : V_VIN; break;
	case hwmon_curr: idx = (ch == 1 || ch == 2) ? V_IOUT : V_IIN; break;
	case hwmon_power: idx = (ch == 1 || ch == 2) ? V_POUT : V_PIN; break;
	default: return -EOPNOTSUPP;
	}
	mutex_lock(&vrm_lock);
	vrm_update();
	raw = vrm_raw[page][idx];
	if (!vrm_ok[page][idx]) {
		mutex_unlock(&vrm_lock);
		return -ENODATA;
	}
	mutex_unlock(&vrm_lock);

	switch (idx) {	/* HWiNFO validity limits and LSBs */
	case V_T1: case V_T2:
		if (!raw || raw > 200) return -ENODATA;
		*val = raw * 1000; break;			/* 1 C */
	case V_VOUT:
		if (raw < 0x0b || raw > 0xffef) return -ENODATA;
		*val = raw; break;				/* 1 mV */
	case V_VIN:
		*val = raw * 10; break;				/* 10 mV */
	case V_IOUT:
		if (raw >= 0xfd00) return -ENODATA;
		*val = raw * 100; break;			/* 0.1 A -> mA */
	case V_IIN:
		if (raw >= 0xfd00) return -ENODATA;
		*val = raw * 10; break;				/* 10 mA */
	default:
		if (!raw || raw > 500) return -ENODATA;
		*val = raw * 1000000L; break;			/* 1 W -> uW */
	}
	return 0;
}

static const char *const vrm_labels[4][4] = {
	{ "VR Loop1", "VR Loop2" },
	{ "VR VIN", "VR VOUT (VDDCR_CPU)", "VR VOUT (VDDCR_SOC)" },
	{ "IIN (VDDCR_CPU)", "IOUT (VDDCR_CPU)", "IOUT (VDDCR_SOC)", "IIN (VDDCR_SOC)" },
	{ "PIN (VDDCR_CPU)", "POUT (VDDCR_CPU)", "POUT (VDDCR_SOC)", "PIN (VDDCR_SOC)" },
};

static int vrm_read_string(struct device *dev, enum hwmon_sensor_types type,
			   u32 attr, int ch, const char **str)
{
	int t = type == hwmon_temp ? 0 : type == hwmon_in ? 1 : type == hwmon_curr ? 2 : 3;

	*str = vrm_labels[t][ch];
	return 0;
}

static umode_t vrm_visible(const void *d, enum hwmon_sensor_types type, u32 attr, int ch)
{
	return 0444;
}

static const struct hwmon_channel_info * const vrm_info[] = {
	HWMON_CHANNEL_INFO(temp, HWMON_T_INPUT | HWMON_T_LABEL, HWMON_T_INPUT | HWMON_T_LABEL),
	HWMON_CHANNEL_INFO(in, HWMON_I_INPUT | HWMON_I_LABEL, HWMON_I_INPUT | HWMON_I_LABEL,
			   HWMON_I_INPUT | HWMON_I_LABEL),
	HWMON_CHANNEL_INFO(curr, HWMON_C_INPUT | HWMON_C_LABEL, HWMON_C_INPUT | HWMON_C_LABEL,
			   HWMON_C_INPUT | HWMON_C_LABEL, HWMON_C_INPUT | HWMON_C_LABEL),
	HWMON_CHANNEL_INFO(power, HWMON_P_INPUT | HWMON_P_LABEL, HWMON_P_INPUT | HWMON_P_LABEL,
			   HWMON_P_INPUT | HWMON_P_LABEL, HWMON_P_INPUT | HWMON_P_LABEL),
	NULL
};

static const struct hwmon_ops vrm_ops = {
	.is_visible = vrm_visible,
	.read = vrm_read,
	.read_string = vrm_read_string,
};

static const struct hwmon_chip_info vrm_chip = { .ops = &vrm_ops, .info = vrm_info };

/* RAA229004 answers PMBUS_REVISION 0x33 (PMBus 1.3); HWiNFO pages only then. */
static bool vrm_present(void)
{
	u64 rev = 0, page = 0;
	bool ok = !gsa1_call("SMM3", VRM_ADDR, 0x98, &rev) && rev == 0x33;

	gsa1_call("SMM3", VRM_ADDR, 0x00, &page);
	pr_info("gsa1_smbus: VRM 0x60 PMBUS_REVISION %#llx, PAGE %llu%s\n", rev, page,
		ok ? "" : " (not RAA229004-like, VRM sensors off)");
	return ok;
}

static s32 gsa1_xfer(struct i2c_adapter *a, u16 addr, unsigned short flags,
		     char read_write, u8 command, int size,
		     union i2c_smbus_data *data)
{
	u64 v;
	int ret;

	if (addr < TSOD_FIRST || addr > TSOD_LAST)
		return -ENXIO;
	if (read_write != I2C_SMBUS_READ) {
		if (size != I2C_SMBUS_WORD_DATA ||
		    command < TSOD_REG_CONFIG || command > TSOD_REG_CRIT)
			return -EPERM;
		ret = gsa1_call4("SMM4", addr, command, data->word, 4, &v);
		if (ret)
			return ret;
		return v == 1 ? 0 : -EIO;
	}

	switch (size) {
	case I2C_SMBUS_BYTE_DATA:
		ret = gsa1_call("SMM3", addr, command, &v);
		if (ret)
			return ret;
		if (v > 0xff)
			return -ENXIO;
		data->byte = v;
		return 0;
	case I2C_SMBUS_WORD_DATA:
		ret = gsa1_call("SMM5", addr, command, &v);
		if (ret)
			return ret;
		if (v > 0xffff)
			return -ENXIO;
		data->word = v;		/* DAT1 << 8 | DAT0, as SMBus wants */
		return 0;
	default:
		return -EOPNOTSUPP;
	}
}

static u32 gsa1_func(struct i2c_adapter *a)
{
	/* jc42 checks for BYTE_DATA | WORD_DATA; gsa1_xfer narrows writes */
	return I2C_FUNC_SMBUS_BYTE_DATA | I2C_FUNC_SMBUS_WORD_DATA;
}

static const struct i2c_algorithm gsa1_algo = {
	.smbus_xfer	= gsa1_xfer,
	.functionality	= gsa1_func,
};

static const struct dmi_system_id gsa1_dmi[] = {
	{ .matches = {
		DMI_MATCH(DMI_BOARD_VENDOR, "Gigabyte Technology Co., Ltd."),
		DMI_MATCH(DMI_BOARD_NAME, "B550 AORUS ELITE V2"),
	} },
	{ }
};

static int __init gsa1_init(void)
{
	acpi_handle h;
	int i, ret;

	if (!dmi_check_system(gsa1_dmi))
		return -ENODEV;
	if (ACPI_FAILURE(acpi_get_handle(NULL, "\\GSA1", &gsa1)) ||
	    ACPI_FAILURE(acpi_get_handle(gsa1, "SMM5", &h)) ||
	    ACPI_FAILURE(acpi_get_handle(gsa1, "SMM4", &h)) ||
	    ACPI_FAILURE(acpi_get_handle(gsa1, "SMM2", &h)) ||
	    ACPI_FAILURE(acpi_get_handle(gsa1, "SMM3", &h)))
		return -ENODEV;

	adap.owner = THIS_MODULE;
	adap.algo = &gsa1_algo;
	strscpy(adap.name, "GSA1 ACPI SMBus (DIMM TSOD only)",
		sizeof(adap.name));
	ret = i2c_add_adapter(&adap);
	if (ret)
		return ret;

	for (i = 0; i < ARRAY_SIZE(clients); i++) {
		struct i2c_board_info info = { I2C_BOARD_INFO("jc42",
						TSOD_FIRST + i) };
		u64 id;

		/* JC-42.4 manufacturer ID register; absent slots read 0xFFFF.. */
		if (gsa1_call("SMM5", TSOD_FIRST + i, 0x06, &id) || id > 0xffff)
			continue;
		clients[i] = i2c_new_client_device(&adap, &info);
		if (IS_ERR(clients[i]))
			clients[i] = NULL;
	}
	if (vrm_present()) {
		vrm_hwmon = hwmon_device_register_with_info(&adap.dev, "raa229004", NULL,
							    &vrm_chip, NULL);
		if (IS_ERR(vrm_hwmon))
			vrm_hwmon = NULL;
	}
	return 0;
}

static void __exit gsa1_exit(void)
{
	int i;

	if (vrm_hwmon)
		hwmon_device_unregister(vrm_hwmon);
	for (i = 0; i < ARRAY_SIZE(clients); i++)
		i2c_unregister_device(clients[i]);
	i2c_del_adapter(&adap);
}

module_init(gsa1_init);
module_exit(gsa1_exit);
MODULE_DESCRIPTION("ACPI-backed SMBus for B550 AORUS ELITE V2 DIMM sensors and RAA229004 VRM");
MODULE_LICENSE("GPL");
MODULE_ALIAS("dmi:*:rnB550AORUSELITEV2:*");
