/* fos-authored build configuration for the vendored libopus decoder.
 *
 * This is NOT part of upstream libopus (which normally generates config.h
 * via autoconf) -- it is this project's own, hand-written substitute,
 * matching AGENTS.md's "Real-time audio streaming over RTP/UDP" Opus
 * design: a vendored fixed-point-only decoder, no libm, no libc malloc.
 *
 * Vendored from: https://github.com/xiph/opus release v1.5.2
 * (tapes/opus-codec/vendor/opus/, trimmed to the decode-only subset --
 * see the Makefile's opus-codec-lib recipe for the exact file list and
 * AGENTS.md for the rationale). License: BSD-3-Clause, see
 * tapes/opus-codec/vendor/opus/COPYING (kept verbatim from upstream).
 *
 * FIXED_POINT + DISABLE_FLOAT_API: build the fixed-point decoder only --
 * no libm dependency anywhere (verified by inspecting every compiled
 * object's undefined symbols: no sqrt/sin/cos/exp/log/pow appears).
 *
 * CUSTOM_MODES is deliberately left undefined: the non-custom-modes path
 * uses libopus's precomputed static CELTMode tables
 * (celt/static_modes_fixed.h) instead of building a mode at runtime, which
 * is what makes the decoder allocation-free apart from the one top-level
 * decoder-state buffer (see OVERRIDE_OPUS_ALLOC below) -- every
 * opus_alloc/opus_free call inside celt/modes.c, celt/mdct.c, celt/rate.c,
 * and celt/kiss_fft.c lives inside "#ifdef CUSTOM_MODES" and is therefore
 * dead code here.
 *
 * VAR_ARRAYS: selects plain C99 variable-length arrays for libopus's
 * internal scratch/stack_alloc.h mechanism, rather than USE_ALLOCA (an
 * extra compiler-intrinsic dependency) or NONTHREADSAFE_PSEUDOSTACK (a
 * second, hidden allocator). VLAs need no support beyond what -ffreestanding
 * already provides.
 *
 * CUSTOM_SUPPORT / OVERRIDE_OPUS_ALLOC / OVERRIDE_OPUS_FREE:
 * celt/os_support.h's default opus_alloc()/opus_free() call libc
 * malloc()/free() directly, which this freestanding target does not have.
 * CUSTOM_SUPPORT makes os_support.h #include our own custom_support.h
 * (upstream's documented extension point), which declares opus_alloc/
 * opus_free's prototypes and sets OVERRIDE_OPUS_ALLOC/OVERRIDE_OPUS_FREE to
 * suppress libopus's own static-inline malloc-backed definitions;
 * tapes/opus-codec's Rust wrapper (src/lib.rs) then provides the real
 * opus_alloc/opus_free symbols, backed by the tape ABI's own alloc/free
 * callbacks. Only these two are overridden -- opus_realloc/
 * opus_alloc_scratch are never referenced by the compiled decode-only
 * object set (confirmed via `nm -u`), so their libc-backed defaults are
 * simply never linked in.
 *
 * One more libc symbol, plain `abs()`, is also provided by
 * tapes/opus-codec's Rust wrapper (a few vendored files, e.g. cwrs.c and
 * pitch.c, call it): GCC normally substitutes its own builtin for `abs`
 * automatically, but this build's `-fno-builtin` flag (see the root
 * Makefile's OPUS_CODEC_CFLAGS) disables that substitution, leaving a
 * real undefined symbol that only surfaces once the freestanding
 * i686-fos build is actually linked -- a host build always has libc's
 * own `abs` to satisfy it instead.
 */
#ifndef CONFIG_H
#define CONFIG_H

#define OPUS_BUILD 1
#define FIXED_POINT 1
#define DISABLE_FLOAT_API 1
#define VAR_ARRAYS 1
#define CUSTOM_SUPPORT 1

#endif
