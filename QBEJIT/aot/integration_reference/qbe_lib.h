/* QBE Library API - Minimal interface for embedding QBE
 * 
 * Example:
 *   qbe_compile_file("input.qbe", "output.s", NULL);
 *   // or:
 *   qbe_compile_string(qbe_il_buffer, output_fp, "amd64_apple");
 */

#ifndef QBE_LIB_H
#define QBE_LIB_H

#include <stdio.h>

/* Compile QBE IL file to assembly file
 * target: "amd64_sysv", "amd64_apple", "arm64", etc. (NULL = default)
 * Returns 0 on success, -1 on error
 */
int qbe_compile_file(const char *input_path, const char *output_path, const char *target);

/* Compile QBE IL string to FILE*
 * Returns 0 on success, -1 on error
 */
int qbe_compile_string(const char *qbe_il, FILE *output, const char *target);

#endif /* QBE_LIB_H */
