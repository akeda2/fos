#ifndef A64_STDLIB_H
#define A64_STDLIB_H
#include <stddef.h>
void *malloc(size_t);
void free(void *);
void *realloc(void *, size_t);
void *calloc(size_t, size_t);
int abs(int);
#endif
