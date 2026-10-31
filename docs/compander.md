# Compander: dbx Type I/II (crates/compander, tapes/compander)

A single-band 2:1 compressor / 1:2 expander in the dbx manner: the record side (`Mode::Encode`) compresses, the playback
side (`Mode::Decode`) expands, `Mode::Bypass` passes samples. One engine, two families (`Params::type_i()`,
`Params::type_ii()`), reusable by any tape (`no_std`, allocation-free, hard-float; log and pow come from `dsp::math`).

## How it works

- **Detector** (Blackmer, US 3,789,143, expired): true RMS in the log domain. A pair of all-pass networks 90 degrees apart
  (`analytic.rs`, eight poles; phase within about 1 degree and ripple at most 0.1 dB from 30 Hz to 20 kHz at 48 kHz) feeds two
  squarers whose sum is ripple-free power, so low tones do not modulate the gain (0.15% or less at 30-100 Hz). Then a short
  smoother, dB, and a slew limit: the level may rise fast and falls at 60 dB/s, the patent's typical recovery rate.
- **Gain** is linear in dB and depends only on the level of the signal it acts on: encode `-(1 - 1/r)(L - pivot)`, decode
  `(r - 1)(L - pivot)`, r = 2, pivot -12 dBFS RMS. decode(encode(x)) is therefore the identity for steady signals (measured within
  0.01 dB from 60 Hz to 10 kHz and -80 to -6 dBFS, both types); on fast changes the two detectors see slightly different signals,
  as in the analogue system.
- **Pre/de-emphasis** in the audio path (370 Hz zero, 1590 Hz pole, a bilinear shelf; the decoder uses the exact inverse).
- **Type II** adds a detector side chain only: high-pass 60 Hz, low-pass 10 kHz and a 440 Hz / 4.8 kHz emphasis. Type I's detector
  hears the whole spectrum. The types are not compatible: decode with the type you encoded with.
- A burst out of silence overshoots at the encoder for a few samples (the analogue units clip or limit there too); the `compander` tape
  limits at full scale and counts the limited samples. In silence the encoder gain grows to about +54 dB (floor -120 dBFS) and
  the decoder takes it off again.
- dbx needs no level calibration, but alignment tones must not be encoded.

## Provisional parameters

The exact Type I / Type II corner frequencies and the pivot level were **not found** in the sources below (the 140X manual shows
them only as graphs; the 150X service manual was not read). The emphasis and detector corners are from Peissig et al., DAFx'04,
"Digital Emulation of Analog Companding Algorithms" (which does not say which type), with the detector filters assigned to Type II by
reading the 140X manual. The pivot follows the manual's remark that encoded peaks are about half the source. `Params` is public so
the values can be refined against a service manual or real dbx-encoded recordings; a dbx unit that is out of spec is not a reference.

Sources: US 3,789,143 (Blackmer, 1971); dbx 140X Type II manual; the DAFx'04 paper above.

## The tape

`compander [type=1|2] [mode=bypass|encode|decode] [device=N] [outdev=N] [screen=N] [rate=HZ] [latency=MS] [in=N] [out=N] [theme=...]`
runs a stereo unit on one duplex device: input and output peak bars, a gain bar and readout per channel, touch/mouse buttons for
Bypass / Encode / Decode and Type I / II (keys b e d 1 2 t). Record: line in -> Encode -> out to the tape machine. Playback: machine
-> Decode -> amplifier. `make test-compander` covers start, arguments, picture, keys and a clean stop on QEMU's HD Audio (silence in);
the sound, latency and the match with real dbx tapes need real hardware.

## Tape equalisation, tone and trim

`crates/compander/src/tapeeq.rs` converts between the NAB and IEC (CCIR/DIN) reel-to-reel curves at 3.75, 7.5, 15 and 30 ips, and at cassette speed (1.875 ips) between 120 us (ferric, Type I; `nab`/`120`) and 70 us (chrome/metal; `iec`/`70`), T1 3180 us for both. The EQ bar under the header always says which is in use: speed, tape and machine standard, and the correction at 50 Hz and 10 kHz (green active, amber when Bypass suppresses it, dim when the standards match).
Decode corrects playback of a tape recorded to the other standard (`C = (s+1/T1s)/(s+1/T1m) * (1+sT2m)/(1+sT2s)`); Encode applies
the exact inverse so the tape plays right on a machine of the other standard. It sits outside the compander (Encode: dbx then
record EQ; Decode: playback EQ then dbx) and is off in Bypass or when machine == tape. Time constants (NAB T1 3180 us, T2 90/50/50/17.5 us;
IEC T1 infinite, T2 90/70/35/17.5 us) are commonly published values, not read from the standards; IEC's infinite T1 uses a 10 Hz
corner. The calibration tone (100 Hz, 1 kHz, 10 kHz sine or pink noise, -40..0 dBFS where 0 is a full-scale sine and pink has that sine's RMS, the scale of `ltas`) goes straight out, past compander, EQ and trim. The
output trim (+-12 dB, 0.5 dB steps) applies in every mode except the tone. The spectrum view (`f`, `view=spectrum`) replaces the strips with a third-octave analyser (`crates/ltas`, 8192-point FFT, 1 s
exponential average, 20 Hz-20 kHz, left green and right amber; bands below the FFT's resolution are dim) of the input (a tape's
return) or the output (`i`, `source=in|out`). Play pink noise through the machine and look at the return: a flat reading is a flat
machine. The meters' detector level is RMS re 1.0 (a full-scale sine reads -3 dB there), so it differs by 3 dB from the tone scale.
The spectrum has two styles (`p`, `style=bars|plot`): third-octave bars, or a line plot of every FFT bin (5.9 Hz, no smoothing between bins; log axis 20 Hz-20 kHz, 110 dB) for spotting overtones. Hold (`h`) freezes the display ("HELD").
Keys: o, f, i, p, h, Left/Right, Up/Down, s, m, r.

## Not done

A record-raw-then-decode-later mode (as `phono` has), a
file-based encode/decode. Dolby A/B/C and the like are trademarked; High Com / CX / Super D style companders are other candidates.
