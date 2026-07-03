/* FasterBASIC Frontend Integration for QBE
 * Compiles BASIC source to QBE IL in memory using embedded compiler
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Forward declare the C++ function from fasterbasic_wrapper.cpp */
extern "C" char* compile_basic_to_qbe_string(const char *basic_path);
extern "C" void set_trace_cfg_impl(int enable);
extern "C" void set_trace_ast_impl(int enable);
extern "C" void set_trace_symbols_impl(int enable);
extern "C" void set_show_il_impl(int enable);

extern "C" {

/* Compile BASIC source file to QBE IL in memory
 * Returns: FILE* to memory buffer containing QBE IL, or NULL on error
 */
FILE* compile_basic_to_il(const char *basic_path) {
    /* Call embedded FasterBASIC compiler */
    char *qbe_il = compile_basic_to_qbe_string(basic_path);
    
    if (!qbe_il) {
        return NULL;
    }
    
    size_t len = strlen(qbe_il);
    
    /* Create FILE* from memory buffer using fmemopen */
    FILE *mem_file = fmemopen(qbe_il, len, "r");
    if (!mem_file) {
        free(qbe_il);
        return NULL;
    }
    
    /* Note: qbe_il memory is now owned by fmemopen */
    return mem_file;
}

/* Check if filename ends with .bas or .BAS */
int is_basic_file(const char *filename) {
    size_t len = strlen(filename);
    if (len < 4) return 0;
    
    const char *ext = filename + len - 4;
    return (strcmp(ext, ".bas") == 0 || strcmp(ext, ".BAS") == 0);
}

/* Enable CFG tracing in the compiler */
void set_trace_cfg(int enable) {
    set_trace_cfg_impl(enable);
}

/* Enable AST tracing in the compiler */
void set_trace_ast(int enable) {
    set_trace_ast_impl(enable);
}

void set_trace_symbols(int enable) {
    set_trace_symbols_impl(enable);
}

/* Enable IL output in the compiler */
void set_show_il(int enable) {
    set_show_il_impl(enable);
}

}  // extern "C"
