# it87-avcc3-dkms-git

[frankcrawford/it87](https://github.com/frankcrawford/it87) plus a `force_avcc3`
parameter. Some boards (seen on a Gigabyte B550 AORUS ELITE V2, IT8688E) leave the
SIO PINX2 "AVCC3 enable" bit clear, so the driver hides in9 (AVCC3), although the ADC
channel reads correctly: 3.072 V here, the same value HWiNFO64 shows on Windows.
The parameter only shows the existing reading and writes nothing to the chip.

```sh
makepkg -si       # replaces it87-dkms-git (provides/conflicts); enables force_avcc3=1
```

Then label it in sensors.d: `label in9 "AVCC3"`.
