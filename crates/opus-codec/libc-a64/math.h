/* Freestanding stand-ins for the few libc headers libopus includes when built with clang for aarch64 (the x86 build uses the host gcc's glibc headers; clang cannot parse those for another target). Only builtins: no libm or libc is linked. */
#ifndef A64_MATH_H
#define A64_MATH_H
#define sqrt __builtin_sqrt
#define sqrtf __builtin_sqrtf
#define fabs __builtin_fabs
#define fabsf __builtin_fabsf
#define floor __builtin_floor
#define floorf __builtin_floorf
#define ceil __builtin_ceil
#define ceilf __builtin_ceilf
#define lrintf __builtin_lrintf
#define lrint __builtin_lrint
#define rintf __builtin_rintf
#define fmaxf __builtin_fmaxf
#define fminf __builtin_fminf
#define copysignf __builtin_copysignf
#define HUGE_VALF __builtin_huge_valf()
#define INFINITY __builtin_inff()
#define NAN __builtin_nanf("")
#define M_PI 3.14159265358979323846
#define isnan(x) __builtin_isnan(x)
#define isinf(x) __builtin_isinf(x)
/* Reached only from code the linker drops (analysis, DRED); declared so it compiles. */
double acos(double);
double cos(double);
double sin(double);
double exp(double);
double log(double);
double log10(double);
double pow(double, double);
#endif
