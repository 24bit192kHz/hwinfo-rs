# HWiNFO64 sensor reverse-engineering: how it reads what Linux doesn't

Target: HWiNFO64 v8.54 portable (`hwi_854.zip`, hwinfo.com, 2026-09-30), `HWiNFO64.exe`
(UPX-packed) → `upx -d` → `HWiNFO64.unpacked.exe`
(sha256 `5ef21625978eb2d2f6906ad31b9817ce8235be80dca556404f1d83e4674e366a`, ImageBase 0x140000000).
Tooling: REA 6.4 + Ghidra on the UPX-unpacked binary, plus objdump-based xref scripts (not included).
Nothing was executed and no hardware was touched.

Method:
- REA 6.4.0 + Ghidra 12.1.4 (`rea-docker analyze`, evidence `ev_318cf094…`, 4 m 34 s), plus REA `decompile` of
  0x14034193c (`ev_fa82608f…`) and 0x1402d8e20 (`ev_827a292a…`), saved as `dec_*.json`. The decompiled C matches the
  objdump reading: for RAA229004, idx 0 → "VR Loop1"/0x8D and idx 1 → "VR Loop2"/0x8E. The IOD formula is
  `(v>>12 & 0xfff)*0.125 - 49.0`.
- For the targeted questions I used `.pdata` function bounds + RIP-relative xrefs (`tools/xref.py`) and objdump
  (`full.dis`), which is far faster on a 26 MB binary.

Tags: **[binary]** = read from HWiNFO64 8.54 code; **[public]** = open-source cross-check; **[inference]**.

---

## a. Renesas RAA229004 CPU VRM (PMBus on FCH SMBus 0xB00, addr 7-bit 0x60 / 8-bit 0xC0)

**Identification [binary]**
- Detection `0x140340cf8` reads, all with read transactions:
  - MFR_ID (0x99) and PMBUS_REVISION (0x98): read-byte (`0x1403c3b60`/`0x1403c3a3c`)
  - MFR_MODEL (0x9A): word/block
  - IC_DEVICE_ID (0xAD): block read (`0x1403c3db4`)
  - 0xA0 and STATUS_BYTE (0x78)
- It maps the IC_DEVICE_ID value through `0x140340628`. **`0x49D26A00` → internal chip id `0xFB229004` (RAA229004)**.
  Siblings: 0x49D25500 → ISL69269, 0x49D23100 → ISL69138, 0x49D2A400 → RAA229629 (etc.).

**Readings [binary]**. All go through one helper, `0x1403c3c80(smbus, port, addr, cmd)`, a plain SMBus
READ WORD (`0x1403c2c9c`). For the RAA229004 / ISL6926x / RAA2291xx family:

| HWiNFO row | PMBus cmd | Decode |
|---|---|---|
| VR Loop1 | 0x8D READ_TEMPERATURE_1 | raw word = °C; invalid if > 200, 0, 0x7FFF, 0xFFFF |
| VR Loop2 | 0x8E READ_TEMPERATURE_2 | same |
| (VR Ambient) | 0x8F READ_TEMPERATURE_3 | same (not shown for this board) |
| VR VOUT | 0x8B READ_VOUT | raw × 0.001 V; invalid if raw < 0x0B or raw > 0xFFEF |
| VR VIN | 0x88 READ_VIN | raw × 0.01 V |
| Current (IOUT) | 0x8C READ_IOUT | raw × 0.1 A; invalid if raw ≥ 0xFD00 |
| Current (IIN) | 0x89 READ_IIN | raw × 0.01 A; invalid if raw ≥ 0xFD00 |
| Power (POUT) | 0x96 READ_POUT | raw = W; invalid if raw > 500, 0 or ≥ 0xFFFE |
| Power (Input) | 0x97 READ_PIN | raw = W; same limits |
| VRM Efficiency | computed | POUT / PIN × 100 (`0x140342a74`, cached at +0x1c0/+0x1c8) |

- Other chips in the same code use LINEAR11 (`0x140340514`), LINEAR16 with the VOUT_MODE exponent (`0x140340574`) or
  DIRECT (`0x1403405c4`: X = (Y·10^−R − b)/m). **The RAA229004 is not one of them**: it is "direct integer" with the
  fixed LSBs above (Renesas native format).
- Constants: 0.001 @0x140920578, 0.01 @0x140920640, 0.1 @0x140920768.

**Writes [binary]**
- None in the detection or sensor paths. The sensor functions 0x14034193c / 0x140341d80 / 0x140342344 /
  0x14034281c / 0x140342a74 call only the read-word helper.
- **PAGE (0x00) is never written.** HWiNFO reads whatever page is selected; VOUT/IOUT/POUT therefore come from the
  current page, normally page 0 = VDDCR_CPU loop. Loop 2 is reached only through READ_TEMPERATURE_2, not by paging.
- Every low-level transaction routine (`0x1403c2730`, `0x1403c21cc`, `0x1403c2c9c`) ORs the read bit into the
  address (`lea r8d,[rbx+rbx]; or r8b,1`). For FCH/PIIX4 the command bytes are 0x48 (byte) and 0x4C (word), i.e.
  START + protocol.

**Linux reimplementation**
- The same reads go through the board ACPI `\GSA1.SMM5(2, 0xC0, cmd)`, the read-word path `gsa1_smbus` already
  uses, extended to allow addr 0x60 with read-word only for the commands {0x8B, 0x88, 0x8C, 0x89, 0x8D, 0x8E,
  0x96, 0x97}.
- **Risk:** the BIOS `_PTS`/`_WAK` (EZV8/EZV7) *does* switch PAGE 0/1 on this VRM (board DSDT/SSDT analysis, not included).
  If a read lands between their PAGE write and restore, you get the other page's value. The firmware mutex `SME0`
  serialises it, and those paths only run during suspend/resume.
- **Confidence:** high for registers and scaling. The `[rbx+0x14]` port index = 0 assumption is high.

---

## b. ASUS ROG STRIX LC RTX 3080 Ti "ITE IT8915FN" (GPU VRM Temperature, GPU Fan %)

**Findings [binary]**
- GPU-board auxiliary chips are handled in `0x140336dbc` (temperatures), `0x14033a8d8` (fans) and detected in
  `0x140333cec`.
- Chip ids are 0xE000xxxx. The GPU-aux chip-name table is **not plaintext** (no "IT8915" string or 0x8915 constant
  anywhere), so I could not tie the "IT8915FN" label to one id with certainty.
- I2C access goes through NVAPI wrappers, all reads:
  - `0x1403fd334` read byte
  - `0x1403fd410` read word
  - `0x1403fd4d0` block read
  - `0x1403fd584`
- These wrap `NvAPI_I2CReadEx`; `NvAPI_I2CWriteEx` is resolved too.
- Ids that produce both "GPU VRM Temperature" and "GPU Fan" are candidates: 0xE0001000/1200/1300/1304/1700/2000/2200/2202/2203.
  - 0xE0001900: reg 0x17 byte, °C (valid < 0x7F, ≠ 0x79).
  - 0xE0001904: reg 0x32, (raw + k)/d.
  - 0xE0002100: reg 0x15, °C (valid < 0xC8).
  - 0xE0001A01/1A05: PMBus 0x8D word.
  - 0xE0010281: reg 0x18 word & 0xFFF × 0.125.

**Linux path [public + system]**
- `libnvidia-api.so.1` has **no I2C entries**, but the nvidia kernel driver already exposes the card's buses as
  `/dev/i2c-0..5` ("NVIDIA i2c adapter 1/3/5/6/7/8 at 6:00.0").
- Next step to pin it down (read-only, as root):
  1. `i2cdetect -r -y N` on each NVIDIA bus. `-r` = read-byte probe, no quick-write.
  2. Look for a device that answers with ITE id bytes 0x89 0x15 (ITE chip-id regs are usually 0x20/0x21 or
     0x58/0x5D).
  3. Match it to the candidate register maps above.
- OpenRGB documents ASUS Aura GPU controllers at 0x29/0x2A/0x67 on these buses, so avoid writing there.
- **Confidence:** low on which id is IT8915FN. High that HWiNFO only reads.

---

## c. GPU rail voltages / powers (NVVDD, FBVDD, SRAM, PCIe +12V, 8-pin #1-3, Misc0-3, Input PP Source)

**[binary]**
- The NVAPI loader `0x140168758` resolves (by name hash) `NvAPI_GPU_PowerMonitorGetInfo/GetStatus`,
  `NvAPI_GPU_PowerDeviceGetInfo`, `NvAPI_GPU_ClientPowerTopologyGetStatus`, `NvAPI_GPU_ClientVoltRailsGetStatus` and
  `NvAPI_GPU_GetVoltageDomainsStatus`, and stores the pointers at 0x140aec6f0…0x140aec850.
- The rail labels ("GPU 8-pin #%d Input", "GPU Misc%d Input", "GPU Input PP Source", "…measured at the input side
  of voltage regulator") are the NVIDIA RM **PMGR power-channel** names. No INA3221 I2C addresses appear in the GPU
  rail code (`0x140336230`).
- Conclusion: HWiNFO gets the rails from **NVAPI PowerMonitor (RM PMGR channels)**, not direct I2C.
- (HWiNFO's id = table[CRC32(name)] (`0x14016843c`), so the numeric ids are not plaintext.)

**Linux [system + public]**
- `/usr/lib/libnvidia-api.so.1` (nvidia-utils 610.57.04) exports `nvapi_QueryInterface` and has a 589-entry id
  table (`linux_nvapi_ids.txt` in scratch). It **contains**:
  - PowerMonitorGetInfo `0xC12EB19E` and PowerMonitorGetStatus `0xF40238EF`
  - PowerDeviceGetInfo `0xDB9ED906`
  - ClientPowerTopologyGetInfo/Status `0xA4DFD3F2`/`0xEDCF624E`
  - ClientVoltRailsGetStatus `0x465F9BCF`
  - VoltVoltRailsGetStatus `0x5D0634EE`
  - GetVoltageDomainsStatus `0xC16C7E2C`
- Ids are from nvoc / NvAPIWrapper / LHM.
- Caveat [public: Skyworks-Neo/nvoc per-rail-power.md]: on one Windows R610 laptop `0xF40238EF` was a stub returning
  −104. It must be probed on this desktop: call GetInfo, then GetStatus, and handle errors gracefully.
- Struct layouts are not in NVIDIA headers; versions are encoded as `sizeof | (ver<<16)`. Start from
  nvoc's `voltrails-family-full-layout-r610.md` and LHM's `NvApi.cs`.
- **Confidence:** medium (source identified; Linux availability of live data unverified).

---

## d. GPU Hot Spot, GPU Core Voltage, GPU Effective Clock, GPU Crossbar Clock

**[binary]** NVAPI names resolved: `NvAPI_GPU_ThermChannelGetStatus` (+GetInfo), `NvAPI_GPU_ClientVoltRailsGetStatus`,
`NvAPI_GPU_ClockCounterMeasureAvgFreq`, `NvAPI_GPU_GetAllClockFrequencies`, `NvAPI_GPU_RegisterOp`.

**Linux, two options:**

1. **NVAPI on Linux** (`libnvidia-api.so.1`). The table includes:
   - ThermChannelGetStatus `0x65FE3AAD` [public: LHM "GetThermalSensors" and rtx-monitor: 168-byte v2 struct,
     channel mask, temps as signed 24.8 fixed in words 10/11, channel 1 = Hot Spot].
   - ThermChannelGetInfo `0x0BC8163D`
   - ClientVoltRailsGetStatus `0x465F9BCF` (core voltage, µV)
   - ClockCounterMeasureAvgFreq `0xFB8F61EC` (effective clock)
   - GetAllClockFrequencies `0xDCB616C3` (struct v3, domain 2 = xbar? check)
2. **Hot spot without NVAPI** [public: VACInc/nvidia-gpu-sensors-beszel, validated on GA102 3090]:
   - BAR0 NV_THERM words `0x0203B8, 0x0203BC, 0x02046C, 0x020470, 0x020478, 0x0204B8, 0x0204BC, 0x0204C0,
     0x0204C4`.
   - Valid if `(w>>16)==0x4000` and byte1 ∉ {0, 0xFF}; temp = (w & 0xFFFF)/256; hot spot = max.
   - Exclude 0x020074 and 0x020858 (constant limits).
   - Device id 0x2208 (3080 Ti) is listed there for VRAM 0xE2A8. It slots straight into `kmod/nvvram` as a second
     channel, read-only.
3. **Core voltage without NVAPI** [public, same tool]:
   - RM control `NV2080_CTRL_CMD_VOLT_VOLT_RAILS_GET_INFO/STATUS` (0x2080b201/0x2080b202) via `/dev/nvidiactl`
     ioctls (NV_ESC_RM_ALLOC / RM_CONTROL).
   - Read-only, userspace, no root.

**Confidence:** high for the hotspot register method (public + device-id matched); medium for the NVAPI struct layouts.

---

## e. Zen 3 "Enhanced" extras

| HWiNFO row | Source [binary] | Decode |
|---|---|---|
| CPU IOD Hotspot / CPU IOD Average | `0x1402d8e20`: **SMN 0x59824 + 4·i** (die 0) / **0x598B4 + 4·i** (die 1) when any family flag at obj+0x210/0x214/0x218 is set (inference: Zen2/3 desktop); else 0x59888/0x598A4/0x59914/0x59834 (die 1: 0x599A4/0x599C0/0x59A30/0x598C4). i = 0..31, gated by the present-mask at obj+0x300/0x304 | default: valid if bit 11, T = ((v>>12)&0xFFF) × 0.125 − 49 °C; if flags +0x208/0x21C/0x228/0x230 are set: valid bit 10, (v>>11)&0xFFF. Hotspot = max, Average = mean (`0x1402e26a2` loop). On Linux: probe all 32 and keep the valid ones |
| CPU (Tctl) | SMN **0x59800** (`0x1402d718f`) | T = (v>>21) × 0.125, −49 if range-select (bit 19 / 0x30000 rules), as k10temp |
| Thermal Throttling (PROCHOT EXT) | SMN **0x59804** cached at +0x308 (`0x1402d7207`) | bit 2 |
| Thermal Throttling (PROCHOT CPU) | same | bit 3 |
| Thermal Throttling (HTC) | same | bit 4 |
| DRAM Read / Write Bandwidth | PM table float, per-version offset: read ∈ {0x10C, 0x120, 0x2F8, 0x320, 0x32C}, write = read + 4 (`0x140323f6e…0x140324213`, `0x140322df1`) | GB/s as-is |
| Power Reporting Deviation | PM-table ratio × 100 (`0x14031a69c`), version-specific | not fully recovered for 0x380805 |

- **DRAM bandwidth on 0x380805**: for Vermeer the candidates are 0x10C/0x110 (idx 67/68) or 0x110/0x114
  (ryzen_monitor's CS_UMC_READS/WRITES = idx 68/69). Verify on Linux by watching both under a memory-bandwidth load
  (read-only).
- **SMN on Linux:** the kernel's amd_nb (`amd_smn_read`, root via debugfs), or ryzen_smu's `/sys/kernel/ryzen_smu_drv/smn`
  (write the address to the file, read back the value). Both are reads of the SMN index/data pair, the same thing HWiNFO
  does through its driver (`0x1400262b0` → `0x1401ceb08`, PCI index/data).
- **Confidence:** high for IOD and throttle bits; medium for the bandwidth offset; low for PRD.

---

## f. Memory timings (Zen 3, DDR4), SMN UMC

**[binary]** `0x140208da3…0x140209520`: base `0x50000 + ch·0x100000`, reads of +0x100, +0x200, +0x204…+0x230,
+0x260/+0x264/+0x268 and +0x2C0…, +0xDF0/0xDF4, +0xD6C. The bitfields match ZenTimings exactly **[public:
irusanov/ZenTimings MainWindow.xaml.cs]**:

| Field | SMN | Bits |
|---|---|---|
| MemClk ratio | 0x50200 | r = [6:0] / 3; MCLK = r × 100 MHz, DDR rate = r × 200 MT/s |
| Cmd2T / GDM | 0x50200 | bit 10 (1 = 2T) / bit 11 |
| tCL | 0x50204 | [5:0] |
| tRAS | 0x50204 | [14:8] |
| tRCDRD / tRCDWR | 0x50204 | [21:16] / [29:24] |
| tRC | 0x50208 | [7:0] |
| tRP | 0x50208 | [21:16] |
| tRFC (DDR4) | 0x50260 (or 0x50264 if 0x50260 == 0x21060138) | [10:0]; RFC2 [21:11]; RFC4 [31:22] |
| channel B | +0x100000 (0x150xxx) | |

Writes: none (pure SMN reads). **Confidence:** high.

---

## Summary: what to build next on Linux (all read-only)

1. **VRM:** extend `gsa1_smbus` to addr 0x60, read-word only, for 0x8B/0x88/0x8C/0x89/0x8D/0x8E/0x96/0x97, with the
   scaling above. 9 HWiNFO rows.
2. **GPU hot spot:** add the BAR0 NV_THERM max channel to `kmod/nvvram`.
3. **GPU core voltage, effective clock, rails:** NVAPI on Linux (`libnvidia-api.so.1`) or RM ioctls. Probe
   PowerMonitor support first.
4. **IOD Avg/Hotspot, HTC/PROCHOT, memory timings:** SMN reads via ryzen_smu `smn` or debugfs.
5. **IT8915FN:** first identify the chip on `/dev/i2c-0..5` with `i2cdetect -r` (reads only), then add it.

---

## a2. Second RAA229004 instance (VDDCR_SOC): correction to (a)

**Correction:** HWiNFO *does* write PAGE (0x00). I missed it in the first pass because the write isn't in the sensor
or detection functions; it is in the class's per-poll "begin update" virtual method. [binary]

**Class and instances**
- VR chip class vtable `0x1408ce8b0`: `[0]` ctor/dtor 0x140340cb8, then the read methods 0x14034193c (temps),
  0x140341d80 (VIN/VOUT), 0x140342344 (IIN/IOUT), 0x14034281c (PIN/POUT), 0x140342a74 (efficiency),
  **0x1403417e0 (begin-update)**, **0x140341920 (end-update)**, and 0x140340cf8 (detect).
- Instances are created in `0x1403569cc` at `0x140357f6d…0x14035809b`: **96 objects**, r15 = 0..95.
  - `+0x1b0` (page/instance index) = r15 / 48, so **0 or 1**.
  - `+0x14` (SMBus port) = (r15 mod 48) / 16, so 0..2.
  - `+0x10` (7-bit address) = table[r15 mod 16] at `0x1408411e0`.

**(1) Address scan list** (7-bit, per port, per page index):
`0x70 0x71 0x72 0x73 0x74 0x75 0x76 0x77 0x20 0x2B 0x40 0x41 0x42 0x60 0x61 0x62`.
On this board both RAA229004 groups are **addr 0x60 (8-bit 0xC0), port 0**, with page index 0 and 1.

**(2) How the second instance is selected**
- Not a different address, and not PAGE_PLUS_READ. Before every poll, begin-update `0x1403417e0` does:
  - Takes the global SMBus lock (`0x140217f70(0)`). The paired release is `0x1402180f8` in end-update
    `0x140341920`.
  - If `+0x1b8` (multi-page capable) != 0 and `+0x1b0` <= 1, calls `0x1403c3bfc(smbus, addr=+0x10, port=+0x14, cmd=0x00, data=(u8)+0x1b0)`.
- `0x1403c3bfc` → `0x1403c1914`, verified on the FCH (piix4-style) branch:
  - HCMD (base+3) = cmd
  - TXSA (base+4) = **addr<<1 with no read bit**
  - DAT0 (base+5) = data
  - HCNT (base+2) = 0x48
- That is **SMBus WRITE BYTE DATA: PAGE = 0 or 1.**
- Multi-page gate (detect `0x140340ed0`): `+0x1b8 = (PMBUS_REVISION(0x98) == 0x33)`, i.e. PMBus 1.3, or the special
  0x78 STATUS == 0x25xx case. If it is not multi-page and `+0x1b0` > 0, the instance is dropped (`0x140340f3b`).
- The RAA229004 is PMBus 1.3, so page index 1 survives and becomes group #2 (VDDCR_SOC).
- Also in begin-update, only for ids 0xFB229620/621/628 and 0xFB229130/131: a word write via `0x1403c1d60`
  (0xED3F/0xED40, the Renesas DMA-address path) followed by a read (`0x1403c321c`). **Not executed for the
  RAA229004 (0xFB229004).**

**(3) Transactions per polling cycle on SMBus port 0, addr 0x60**
- Detect (once per instance, no PAGE write, so it reads the current page):
  1. RD_BYTE 0x99
  2. RD_BYTE 0x98
  3. RD_WORD 0x9A
  4. RD_WORD 0x99
  5. BLOCK_RD 0x9A, 0x99
  6. RD_BYTE 0x98
  7. BLOCK_RD 0xAD
  8. RD_BYTE 0xA0
  9. RD_WORD 0x78
  10. RD_WORD 0x99

  Plus RD_BYTE 0x3E/0x3D at addr 0x20 for some chips.
- Instance #1 (VDDCR_CPU):
  - lock
  - **WR_BYTE cmd 0x00 data 0x00**
  - RD_WORD 0x8D, 0x8E (temps)
  - RD_WORD 0x8B (VOUT), 0x88 (VIN)
  - RD_WORD 0x8C (IOUT), 0x89 (IIN)
  - RD_WORD 0x96 (POUT), 0x97 (PIN)
  - unlock
- Instance #2 (VDDCR_SOC): lock, **WR_BYTE cmd 0x00 data 0x01**, then the same reads, then unlock.
- **PAGE is not restored afterwards.** It stays at 1 after each cycle.
- That is why Loop1/Loop2 temps and VIN read the same in both groups: those values are chip-wide, not per rail.

**Scaling** is the same as in (a) for both pages: VOUT ×0.001 V, VIN ×0.01 V, IOUT ×0.1 A, IIN ×0.01 A, POUT/PIN W, temps °C.

**Implications for Linux**
- To get the SOC rail you must write PAGE (0x00) = 1 and read, and you should restore PAGE = 0.
- Do it inside the firmware mutex: use `\GSA1.SMM2`/`SMB2`, the write-byte AML, and `SMM5` for reads. Each ACPI
  call takes and drops `SME0` on its own, so a PAGE-write + read sequence is *not* atomic against `_PTS`/`_WAK`.
  Those run only during suspend/resume.
- PAGE is the only write needed. 0x00 is the PMBus PAGE register; it changes nothing on the VRM output.
- Writing any other command code at 0xC0 is unnecessary and should stay blocked.

---

## Round 3

### R3.1 Power Reporting Deviation (Accuracy), pm_table 0x380805 [binary]
Code: function `0x140308b6c`, sensor case ending at `0x14031a69c`. Debug format at `0x1408c5ab0`:
`"FSC Bias: %.1f, %.2f, %.2f"` = (ΣcorePower, bias, ratio).

1. **ΣcorePower** (xmm14), loop at `0x140313ee9…0x14031598a`:
   - for ccd in 0..15 (only if the CCD-present flag at `chip+((ccd+0x2ED)*16)` != 0), for core in 0..7:
     `+= pm_f32[byte 0x2B0 + 4*(core + 8*ccd)]`.
   - For 0x380805 this is **CORE_POWER, float idx 172..187**. Branch `0x14031432c`: versions ending …05
     (0x380005/105/205/605/705/805) use 0x2B0; versions ending …04 use 0x2A4 = idx 169.
   - Each term is NaN-/range-clamped to 0 (`xmm12` = FLT_MAX, `xmm13` = −FLT_MIN).
2. **Bias** (xmm11, Vermeer block `0x140318800…0x140318b15`):
   `pm[1] / (pm[47] + ΣcorePower + pm[28] + pm[27] + pm[26])`.
   In ryzen_monitor names that is PPT_VALUE / (SOC_TELEMETRY_POWER + ΣCORE_POWER + ROC_POWER + VDD18_POWER +
   VDDIO_MEM_POWER), byte offsets 0x4 / (0xBC, 0x70, 0x6C, 0x68). Used only as a validity gate (must be ≥ 0).
3. **Ratio** (xmm10, `0x140318b1a` → `0x14031644d` → `0x14031a649`):
   `pm[43] / ΣcorePower`, where pm[43] = byte 0xAC = **CPU_TELEMETRY_POWER**, i.e. SVI2 TFN core power as the SMU
   sees it. This is the "CPU Core Power (SVI2 TFN)" row, *not* the external VRM's IOUT/POUT.
4. **PRD = 100 × pm[43] / Σ pm[172..187]**:
   - Shown only if both bias ≥ 0 and ratio ≥ 0 (`0x14031a67e`).
   - No smoothing or inversion; HWiNFO's normal min/max/avg apply.
   - Default 0 if ΣcorePower == 0. Meaningful only under heavy all-core load, per HWiNFO's own tooltip.

### R3.2 GPU "Performance Limit - …" [binary + public NvAPIWrapper]
- **Call:** `NvAPI_GPU_PerfPoliciesGetStatus`, id **0x3D358A0C**, **present in Linux libnvidia-api** (also GetInfo
  0x409D9841). HWiNFO slot 0x140aec798, call at `0x14033d9d6`.
- **Struct v1, 0x550 bytes** (= NvAPIWrapper `PrivatePerformanceStatusV1`):
  - +0x00 u32 version = **0x00010550**
  - +0x04 u32 **input mask = 0x1F** (HWiNFO sets it; NvAPIWrapper leaves 0)
  - +0x08 u64 timer ns (HWiNFO treats 0 as no data)
  - **+0x10 u32 limit bitmask**
  - +0x14…: unknowns, 3×u64 timers at +0x20, then 326×u32
- **Bit mapping** (`0x14033da6d…0x14033db81`, bit index = sensor case):

  | Bit | Row |
  |---|---|
  | 0 | Power |
  | 1 | Thermal |
  | 2 | Reliability Voltage |
  | 3 | Max Operating Voltage |
  | 4 | Utilization |
  | 5 | SLI GPUBoost Sync |

- The parent "GPU Performance Limiters" row is the OR / any-set.
- HWiNFO also calls `NvAPI_GPU_GetPerfDecreaseInfo_Internal` (v1, size 0xC, version 0x0001000C) but only logs it
  (`"nvPerfDec=%X, %X"`). Not needed. Note 0x7F7F4600 (public GetPerfDecreaseInfo) is **absent** on Linux.

### R3.3 GPU clocks [binary]
Updater `0x1403e07bc`; cached fields on the GPU object are mapped to labels in `0x14033e908`. Divisor xmm6 = 1000
(kHz → MHz).

**"GPU Effective Clock"** (+0xD18) comes from `NvAPI_GPU_GetAllClocks` (id **0x1BD69F49**, present on Linux), *not*
ClockCounter:
- Struct **v2, 0x484 bytes, version 0x00020484**, zero-filled, single call (`0x1403e40ca`).
- If `(u8 @+0x2AC) & 0x20`: effective = `u32 @+0x2A8 / 1000 × 0.5` MHz; else `u32 @+0x104 / 1000` MHz.
- HWiNFO also dumps 32 × 8-byte entries at +0x04 and 32 × 0x1C-byte entries at +0x108 to its debug log.

**"GPU Crossbar Clock"** (+0xD70) = GetAllClocks `u32 @+0x0C / 1000`, i.e. the +0x04 array entry 1, value word.
"GPU System Domain Clock" (+0xD80) = `u32 @+0x14 / 1000` (entry 2).

**"(measured)" clocks**: `NvAPI_GPU_ClockCounterMeasureAvgFreq` (id **0xFB8F61EC**, present on Linux), `0x1403e41f7…0x1403e42f5`:
- Three structs, each **v3, 0x178 bytes**, zero-filled, then:
  - +0x00 u32 = **0x00030178**
  - +0x04 u32 = clock domain: **0 = graphics → "GPU Clock (measured)", 4 = memory → "GPU Memory Clock (measured)",
    1 = crossbar → "GPU Crossbar Clock (measured)"**
  - +0x0B u8 = **1**
  - +0x10 u8 = **0xFF**
- **Two-phase:**
  1. Call all three (start the counter window), keep each return status.
  2. `Sleep(30 ms)` (`0x1402167e0(0x1E)`).
  3. Call again the same buffers for those that returned 0.
  4. Result = **u32 @+0x14 (kHz)** / 1000.
- Memory (measured) is further divided by 1/2/4/8 depending on the memory-type flags at gpu+0xE7C..0xE90
  (GDDR6X → /2 or /4; compare against NVML mem clock 9876 to pick).
- This explains the all-zeros result when only the version word was set: domain, the +0x0B/+0x10 control bytes and
  the second call are all required.

**Base clocks** come from `NvAPI_GPU_GetAllClockFrequencies` (0xDCB616C3), struct 0x108, version 0x00030108 with
fallback 0x00020108:
- `+0x04 &= ~0xF` (ClockType 0 = current).
- Entries of 8 bytes at +0x08: {u32 flags (bit 0 present), u32 kHz}.

| Entry | Row |
|---|---|
| 0 | GPU Clock (+0xD10) |
| 4 | Memory (+0xD50) |
| 7 | Shader (+0xD48) |
| 8 | Video (+0xD68) |

Crossbar is *not* in GetAllClockFrequencies. That matches the Linux probe that saw only domains 0/4/8.
