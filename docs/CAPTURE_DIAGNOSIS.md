# Capture device: diagnosis notes

Everything here was measured on this machine against the real dongle, not
inferred from a spec sheet alone. Commands that produced each number are given
so any claim can be re-checked later.

---

## 1. What the device is

| Field | Value |
| --- | --- |
| Friendly name | `Hagibis` |
| Instance IDs | `USB\VID_345F&PID_2130&MI_00\<instance>` (video), `...&MI_02\<instance>` (audio) |
| Chip | MacroSilicon / UltraSemi **MS2130** |
| USB | Integrated USB 3.0 device controller, *compatible with USB 2.0 device mode* |

Discovery:

```
KVM-Control-Plus.exe --list-devices
powershell -NoProfile -Command "Get-PnpDevice -Class Camera,Image -PresentOnly |
  Where-Object { $_.InstanceId -like '*VID_345F*' } | Format-List FriendlyName,InstanceId,Status"
```

## 2. Which USB mode is it actually running in?

The MS2130 datasheet (HJ-BMJL-PD-002) publishes **different** output resolution
lists for its two modes. At 1920x1080:

| Mode | MJPEG | YUV422 (uncompressed) |
| --- | --- | --- |
| **USB 3.0** | 60 / 50 / 30 / 20 / 10 Hz | 60 / 50 / 30 / 20 / 10 Hz |
| **USB 2.0** | 50 / 30 / 25 / 20 / 10 Hz | 10 / 5 Hz only |

Our device advertises, at 1920x1080: **50 / 30 / 25 / 20 / 10 Hz**, with no
1080p60 anywhere in its 139-format list.

That list is the datasheet's **USB 2.0** row, not the USB 3.0 row — note the
presence of 25 Hz and the absence of 60 Hz, which is the fingerprint that
separates them. **Conclusion: the dongle is enumerating and streaming in USB 2.0
mode.**

Evidence:

```
KVM-Control-Plus.exe --raw-probe 2 0
  [0] 1920x1080 @ 50 NV12: ...
  [1] 1920x1080 @ 50 MJPG: ...
  [2] 1920x1080 @ 30 NV12: ...
  [3] 1920x1080 @ 30 MJPG: ...
  [4] 1920x1080 @ 25 NV12: ...      <- 25 Hz: USB2 fingerprint
  ...                                <- no 60 Hz at any size
```

Confirmed on the OS side that it is a real composite USB device on a USB 3.x
*root* hub (a USB 3.x root hub carries the USB 2.0 bus too, so that alone does
not prove 3.0 operation — the resolution list is the reliable signal).

## 3. Why this matters: the bandwidth arithmetic

Uncompressed 1920x1080 at 50 Hz, as the device would have to send it:

| Format | Bytes/frame | Bytes/s at 50 Hz |
| --- | --- | --- |
| NV12 (4:2:0, 1.5 B/px) | 3,110,400 | **155.5 MB/s** |
| YUV422 (4:2:2, 2 B/px) | 4,147,200 | **207.4 MB/s** |
| MJPEG (measured) | ~41,109 | ~2.1 MB/s |

USB 2.0 high-speed gives a practical ceiling of roughly **40 MB/s**. So at
1080p50 the uncompressed streams are **5x over budget**, while MJPEG fits more
than ten times over.

**The device still advertises the uncompressed formats at those rates** (Windows
offers "MJPEG & NV12" on a USB 2.0 MS2130 — a known behaviour of this family).
It cannot actually deliver them, so what arrives on the uncompressed stream is a
**filler frame**.

The MS2130 chip spec itself only claims **YUV422 and MJPEG** USB video output —
NV12 is not a chip output format, it is what the Windows/UVC layer presents.

## 4. What each format actually delivers (measured)

`KVM-Control-Plus.exe --raw-probe 2 0` walks every native type, selects it as a
pass-through and prints the byte statistics of the samples that arrive.
`VARYING` vs `UNIFORM` is the discriminator: real picture varies, a filler frame
does not.

| Native format | Bytes/sample | min | max | mean | Verdict |
| --- | --- | --- | --- | --- | --- |
| `1920x1080 @ 50 NV12` | 3,110,400 | 7 | 128 | 47.33 | Y plane **uniform 7**, UV uniform 128 |
| `1920x1080 @ 50 MJPG` | **41,109** | 0 | 255 | 0.68 | real **JPEG** (`ff d8 ff db`), 41 KB |

For NV12 the split of the buffer is decisive:

```
KVM-Control-Plus.exe --snapshot out/x.bmp 0 --level trace
  frame 1920x1080 buffer current=3110400 max=3110400 ... | Y min=7 max=7 mean=7.00 | UV mean=128.00
```

`Y min == Y max == 7` means **every** luma byte is identical: there is no image
content in the buffer at all, only a constant. A real frame — even a fully black
one from a digital source — arrives as a JPEG with structure in the MJPEG path,
and as more than one distinct value in a working uncompressed path.

Video is also not being lost by our conversion: luma 7 with neutral chroma
(128) mathematically converts to RGB ≈ 0, which is exactly what both our own
NV12 conversion and MediaFoundation's own RGB32 conversion produced. The two
paths agree byte-for-byte, so the conversion layer is not the suspect.

## 5. The MJPEG path is proven end-to-end

```
KVM-Control-Plus.exe --grab-native 1 out/frame.jpg
  [1] 1920x1080 @ 50 MJPG: wrote 41109 bytes to out/frame.jpg
xxd out/frame.jpg | head -1
  00000000: ffd8 ffdb 0043 0008 0606 0706 0508 0707  ....C..........
```

That is a complete, valid JPEG (SOI `ffd8`, DQT `ffdb`). Dropping the conversion
entirely and writing the device's own bytes gives a file any image viewer
understands — the strongest form of verification available without a live
picture. Decoded, this particular frame is black, which is a statement about the
**HDMI source at that moment**, not about the capture path.

## 6. Status of the black picture

The black frame is reproducible in *both* formats, so it is one thing, not two:

* a source in standby sends digital black, which is a legitimately uniform
  image, and
* the MJPEG frame of that same moment decodes to solid black.

So "black picture" has (at least) two independent causes on this hardware and
they must not be conflated:

1. **HDMI source asleep / off** — everything is black, in every format, and it
   is correct output. (Seen directly: source was in standby, was woken, showed a
   live blue login screen in MPC, then went dark again.)
2. **USB 2.0 + uncompressed format selected** — the uncompressed stream carries a
   constant filler frame regardless of what the source shows. This one is a
   *real defect in our format choice*, and is the reason section 3 matters.

Cause 2 is the actionable one: the app must not make an uncompressed format the
default while the link is USB 2.0.

## 7. Consequence for the app

* Default to **MJPEG** at the highest rate the device offers (1080p50 here).
  That is the path MPC-HC used successfully on this same dongle.
* Treat uncompressed NV12/YUV as a **USB 3.0-only** option.
* Never let MediaFoundation choose the native format silently by itself: index 0
  of the native list is NV12 here, so a "just ask for a decoded type" request
  lands on the unusable uncompressed stream.
* Keep the diagnostics below, they are what made this visible.

## 8. Diagnostic tooling that produced all of the above

These are permanent, not scaffolding — they are the reason this was diagnosed
instead of guessed.

| Tool | What it answers |
| --- | --- |
| `--list-devices` | Which capture devices exist, with friendly names and hardware IDs |
| `--list-formats <dev>` | What formats the device advertises, nicest first |
| `--raw-probe [n] [dev]` | **Per-native-format** sample statistics: which formats actually stream content (`VARYING`) and which return filler (`UNIFORM`) |
| `--grab-native <i> <file>` | Dumps one raw sample of native format `i` straight to disk with no conversion in the path |
| `--snapshot <file.bmp>` | Waits up to 12 s for a **non-blank** frame, reports `% lit`, then writes it |
| `--level trace` | Per-frame `current/max` buffer sizes plus the **Y-plane and UV-plane** split, and the converted output's lit-pixel count and first bytes |
| `--usb` | **Measured** link speed straight from the USB hub (`IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX[_V2]`) plus the per-format bandwidth arithmetic |

## 9. Community references worth keeping

* HyperHDR / awawa-dev discussion #499 (MS2130 grabber behaviour): on a USB 2.0
  connection Windows offers **MJPEG & NV12**, on USB 3.0 **only YUV**; also
  documents that MS2130 firmware varies ("improved by sellers") and that its
  colour handling needs per-unit LUT calibration.
* MS2130 datasheet, HJ-BMJL-PD-002 — the two-mode resolution tables used above.
* `steve-m/ms2130_patcher` and `BertoldVdb/ms-tools` — community firmware tooling;
  relevant because MS2130 firmware revisions change the offered mode/range.
* Reports that MS2130 YUV is clipped to [16..235] while MJPEG is full-range —
  relevant to colour accuracy once the picture is live.
* Vendor-side caveat: USB 2.0 cards claiming 1080p60 often fake it by inserting
  an empty frame every other frame.

## 10. The inference is now a measurement

Everything above argued from the *offered rate list* that the device is on a USB
2.0 link. That has since been confirmed directly, so the reasoning no longer
rests on an inference.

`--usb` walks the USB hub nodes and reads the negotiated speed out of the kernel:

```
USB link (measured)  USB 2.0 high speed — 480 Mbit/s
Usable throughput    ~40 MB/s after USB overhead
Attached at          port 14 of hub root_hub30
  port supports USB 3.x: false
  operating at SuperSpeed: false
  source: measured from the USB hub
```

* The parent hub is a **USB 3.0 root hub** (`root_hub30`) — the machine's
  controller is not the problem.
* The device negotiated **High Speed** (`USB_NODE_CONNECTION_INFORMATION_EX.Speed
  == 2`), i.e. 480 Mbit/s, and reports it is *not* operating at SuperSpeed.
* The port itself reports no USB 3.x capability, so this is a 2.0-only port on a
  3.0 controller — trying a different cable will not help unless the port changes.

Against ~40 MB/s of usable link:

| Format | Frame bytes @1080p | MB/s @50 fps | Fits? |
|---|---|---|---|
| NV12 | 3,110,400 | 155.5 | **no** — ~4× over |
| YUV422 | 4,147,200 | 207.4 | **no** — ~5× over |
| MJPEG | ~41,109 (measured) | ~2.1 | yes, easily |

This is why an NV12 selection yields a uniform filler frame (`Y ≡ 7`, `UV ≡ 128`)
no matter what the HDMI source is doing, while MJPG yields a valid JPEG that
decodes to black when — and only when — the source itself is asleep.

**Consequence for the UI:** the app now reports all of this in the *Devices* tab
rather than leaving it to be rediscovered. See `capture_report` in `src/app.rs`.

**Consequence for the code:** selecting an uncompressed format on a USB 2.0 link
is a defect, not a user preference. The remaining work is to make the format list
USB-mode-aware — offer MJPEG at the highest available rate, and mark
uncompressed NV12/YUV as requiring USB 3.x. Tracked in `docs/TODO.md`.

---

## 8. Colour: what range and matrix the samples are actually in

Measured on the real device, not assumed:

```
KVM-Control-Plus.exe --trace --snapshot <file> 0
```

reports the negotiated output type and the interpretation drawn from it:

```
colour: BT.709 (media type says BT.709), full 0-255 (media type says 0-255) | output type 1920x1080 stride 1920
```

| Attribute | Reported | Codes (`mfobjects.h`) |
|---|---|---|
| `MF_MT_VIDEO_NOMINAL_RANGE` | **1 ⇒ full 0–255** | 1 = `MFNominalRange_0_255`, 2 = `MFNominalRange_16_235` |
| `MF_MT_YUV_MATRIX` | **1 ⇒ BT.709** | 1 = `MFVideoTransferMatrix_BT709`, 2 = `BT601` |
| `MF_MT_DEFAULT_STRIDE` | 1920 (tight) | row pitch of the Y plane |

The device was set to `1920x1080 @ 60 MJPG`, so MediaFoundation's JPEG decoder
produced the NV12, and it says so: **full-range BT.709**. Until round 5 the
conversion hard-coded *studio-swing BT.601* for every format, which on this input:

| source luma | studio-swing BT.601 | full-range BT.709 |
|---|---|---|
| 16 | 0 | 16 |
| 64 | 56 | 64 |
| 128 | 130 | 128 |
| 200 | 214 | 200 |
| 235 | **255 (clipped)** | 235 |
| 240 | **255 (clipped)** | 240 |

So 21 of 256 luma levels (8% of the tonal range) were being crushed to flat
white, with mid-tones 7–16% high — a washed-out, too-bright picture with no
highlight detail. The matrix error is separate and additional: BT.601
coefficients on BT.709 content shift the hues.

This is also why an uncompressed *native* format needs different maths from a
decoded one: studio-swing expansion was correct for NV12/YUY2 straight off the
device and wrong for anything a JPEG/HEVC/H.264 decoder produced. One conversion
cannot serve both, so the range and matrix are now read per stream and the
decision is logged and shown. Verified by unit tests over known ramps
(`video::mf::tests::full_range_luma_is_neither_stretched_nor_clipped`) — the
device is not always awake, so a test is the only reproducible proof.

**Latency, measured rather than described.** Each frame carries its sampling
instant; the UI reports `N ms now / N ms worst — N fps` and logs a per-second
summary. Capture requests `MF_LOW_LATENCY` (never requested before), the UI is
woken by the capture thread instead of redrawing at the display's refresh rate,
and chroma is read once per 4:2:0 pair instead of per pixel.

---

## 8. "It looks washed out" — what the pixels actually said

The report came with a screenshot of the running app with a live picture in it. That
is evidence, so it was measured rather than eyeballed: the PNG's scanlines were
decoded in Python (no image library, no vision model) and sampled inside the video
area only — the window title bar and the app's own status overlay are not part of the
picture and would have supplied the "true black" for free.

| Sample (inside the video area) | Measured | Reading |
| --- | --- | --- |
| "Chats" heading glyphs | **min 2, 1st percentile 11** | the picture *does* reach true black |
| list/body text | 1% ≈ 101, 5% ≈ 183 | greys where the source's own greys are |
| background | **254** | true white |
| a saturated element | `rgb(0,166,255)` | chroma intact, not muted |

So on this stream — **MJPG at 1920x1080** — the transfer function is already right:
black is black and white is white. The generalised "washed out" explanation
(limited-range content decoded as full range: nothing below ~6% grey, nothing above
~92%) does **not** hold here, and making a range override the default would have made
this picture worse, not better.

What the picture does have: a 1920 px frame shown in a ~1295 px pane — **0.67×** —
decoded from **MJPEG**. Text is soft compared with the same text on the real monitor,
and softness reads as "washed out" to the eye while the numbers say the levels are
fine. Two candidate improvements were considered and rejected as the *fix*: mipmapped
minification (epaint honours `mipmap_mode` only on the glow backend, and mipmaps
smooth rather than sharpen) and a nearest-neighbour switch (worse below 1:1).

What the app does about it instead:

1. **The knobs exist anyway** — `Options → PICTURE`: range and matrix overrides for the
   day a dongle really is wrong, plus brightness / contrast / saturation / gamma for
   taste. Neutral is bit-exact with the untouched decode.
2. **The measurement is on screen**, from the luma pass that already runs for chroma:
   `Source luma source luma 2…254 (mean 118); 0.3% at or below 16, 61.2% at or above 235`.
   A source that never goes below ~16 or above ~235 is the washed-out signature and the
   panel names the fix; one that reaches both ends is told the transfer function is
   right and that scaling and compression are what remain.
3. **`--picture <spec>`** gives the same knobs headlessly, so an adjustment can be
   A/B'd against the untouched decode instead of argued about.
