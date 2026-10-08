/* fos-authored override point for celt/os_support.h's malloc-backed
 * opus_alloc()/opus_free() -- see config.h's CUSTOM_SUPPORT comment.
 *
 * The real definitions live in tapes/opus-codec/src/lib.rs, exported as
 * `#[no_mangle] extern "C" fn`, backed by the tape ABI's alloc/free
 * callbacks (kernel/src/shell.rs's tape_alloc/tape_free) rather than a
 * libc allocator this freestanding target does not have.
 */
#ifndef CUSTOM_SUPPORT_H
#define CUSTOM_SUPPORT_H

#include <stddef.h>

#define OVERRIDE_OPUS_ALLOC 1
void *opus_alloc(size_t size);

#define OVERRIDE_OPUS_FREE 1
void opus_free(void *ptr);

#endif
