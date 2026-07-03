/* FasterBASIC Compiler Integration
 * Runs the full FasterBASIC compilation pipeline and returns QBE IL
 */

#include <string>
#include <sstream>
#include <iostream>
#include <fstream>
#include <cstring>
#include <cstdlib>

#include "fasterbasic_lexer.h"
#include "fasterbasic_parser.h"
#include "fasterbasic_semantic.h"
#include "fasterbasic_cfg.h"
#include "fasterbasic_data_preprocessor.h"
#include "fasterbasic_ast_dump.h"
#include "modular_commands.h"
#include "command_registry_core.h"
#include "codegen_v2/qbe_codegen_v2.h"

using namespace FasterBASIC;

// Global flags for trace options
static bool g_traceCFG = false;
static bool g_traceAST = false;
static bool g_traceSymbols = false;
static bool g_showIL = false;
static bool g_verbose = false;

extern "C" {

/* Compile BASIC source to QBE IL string
 * Returns: malloc'd string with QBE IL, or NULL on error
 */
char* compile_basic_to_qbe_string(const char *basic_path) {
    try {
        // Initialize command registry with core BASIC commands/functions
        static bool registryInitialized = false;
        if (!registryInitialized) {
            auto& registry = FasterBASIC::ModularCommands::getGlobalCommandRegistry();
            FasterBASIC::ModularCommands::CoreCommandRegistry::registerCoreCommands(registry);
            FasterBASIC::ModularCommands::CoreCommandRegistry::registerCoreFunctions(registry);
            FasterBASIC::ModularCommands::markGlobalRegistryInitialized();
            registryInitialized = true;
        }
        
        // Read source file
        std::ifstream file(basic_path);
        if (!file) {
            std::cerr << "Cannot open: " << basic_path << "\n";
            return nullptr;
        }
        std::string source((std::istreambuf_iterator<char>(file)),
                           std::istreambuf_iterator<char>());
        file.close();
        
        // Preprocess DATA statements
        DataPreprocessor dataPreprocessor;
        DataPreprocessorResult dataResult = dataPreprocessor.process(source);
        
        // Debug: Show what DATA preprocessor collected
        if (g_verbose) {
            std::cerr << "[INFO] DataPreprocessor collected " << dataResult.values.size() << " DATA values\n";
            if (!dataResult.values.empty()) {
                std::cerr << "[INFO] DATA values: ";
                for (size_t i = 0; i < dataResult.values.size() && i < 10; ++i) {
                    if (i > 0) std::cerr << ", ";
                    // Print the value based on its type
                    if (std::holds_alternative<int>(dataResult.values[i])) {
                        std::cerr << std::get<int>(dataResult.values[i]);
                    } else if (std::holds_alternative<double>(dataResult.values[i])) {
                        std::cerr << std::get<double>(dataResult.values[i]);
                    } else if (std::holds_alternative<std::string>(dataResult.values[i])) {
                        std::cerr << "\"" << std::get<std::string>(dataResult.values[i]) << "\"";
                    }
                }
                if (dataResult.values.size() > 10) {
                    std::cerr << " ... (" << (dataResult.values.size() - 10) << " more)";
                }
                std::cerr << "\n";
            }
            std::cerr << "[INFO] DATA line restore points: " << dataResult.lineRestorePoints.size() << "\n";
            std::cerr << "[INFO] DATA label restore points: " << dataResult.labelRestorePoints.size() << "\n";
        }
        
        source = dataResult.cleanedSource;  // Use cleaned source
        
        // Lexer
        Lexer lexer;
        lexer.tokenize(source);
        auto tokens = lexer.getTokens();
        
        // Parser
        SemanticAnalyzer semantic;
        semantic.ensureConstantsLoaded();
        
        Parser parser;
        parser.setConstantsManager(&semantic.getConstantsManager());
        auto ast = parser.parse(tokens, basic_path);
        
        if (!ast || parser.hasErrors()) {
            std::cerr << "Parse errors in: " << basic_path << "\n";
            const auto& errors = parser.getErrors();
            for (const auto& error : errors) {
                std::cerr << "  Line " << error.location.line << ": " << error.what() << "\n";
            }
            return nullptr;
        }
        
        // Semantic analysis
        const auto& compilerOptions = parser.getOptions();
        semantic.analyze(*ast, compilerOptions);
        
        if (semantic.hasErrors()) {
            std::cerr << "Semantic errors in: " << basic_path << "\n";
            const auto& errors = semantic.getErrors();
            for (const auto& error : errors) {
                std::cerr << "  " << error.toString() << "\n";
            }
            return nullptr;
        }
        
        // Debug: Dump AST if requested
        if (g_traceAST || getenv("TRACE_AST")) {
            dumpAST(*ast, std::cerr);
            return nullptr;  // Exit after dumping AST
        }
        
        // Debug: Dump symbol table if requested
        if (g_traceSymbols || getenv("TRACE_SYMBOLS")) {
            std::cerr << "\n=== Symbol Table Dump ===\n";
            const auto& symbols = semantic.getSymbolTable();
            
            std::cerr << "\nVariables (" << symbols.variables.size() << "):\n";
            for (const auto& [name, var] : symbols.variables) {
                std::cerr << "  " << name << ": typeDesc=" << var.typeDesc.toString() 
                          << " (isDeclared=" << var.isDeclared << ", isUsed=" << var.isUsed << ")\n";
            }
            
            std::cerr << "\nArrays (" << symbols.arrays.size() << "):\n";
            for (const auto& [name, arr] : symbols.arrays) {
                std::cerr << "  " << name << ": elementTypeDesc=" << arr.elementTypeDesc.toString()
                          << " dimensions=" << arr.dimensions.size() << "\n";
            }
            
            std::cerr << "\nLabels (" << symbols.labels.size() << "):\n";
            for (const auto& [name, label] : symbols.labels) {
                std::cerr << "  " << name << ": labelId=" << label.labelId 
                          << " programLineIndex=" << label.programLineIndex << "\n";
            }
            
            std::cerr << "\nFunctions (" << symbols.functions.size() << "):\n";
            for (const auto& [name, func] : symbols.functions) {
                std::cerr << "  " << name << ": returnTypeDesc=" << func.returnTypeDesc.toString() << "\n";
            }
            
            std::cerr << "=== End Symbol Table ===\n\n";
            return nullptr;  // Exit after dumping symbols
        }
        
        // Build CFG using new single-pass recursive builder
        if (g_verbose) {
            std::cerr << "[INFO] Building complete ProgramCFG (main + all SUBs/FUNCTIONs)...\n";
        }
        CFGBuilder cfgBuilder;
        ProgramCFG* programCFG = cfgBuilder.buildProgramCFG(*ast);
        
        if (!programCFG) {
            std::cerr << "[ERROR] ProgramCFG build failed\n";
            return nullptr;
        }
        
        if (g_verbose) {
            std::cerr << "[INFO] ProgramCFG build successful!\n";
            std::cerr << "[INFO] Main program CFG + " << programCFG->functionCFGs.size() 
                      << " function/subroutine CFGs\n";
            
            // Debug: Show what lines are in the program
            std::cerr << "[INFO] Program has " << ast->lines.size() << " lines\n";
            for (size_t i = 0; i < ast->lines.size() && i < 20; ++i) {
                const auto& line = ast->lines[i];
                std::cerr << "[INFO]   Line " << line->lineNumber << " has " << line->statements.size() << " statements: ";
                for (const auto& stmt : line->statements) {
                    std::cerr << static_cast<int>(stmt->getType()) << " ";
                }
                std::cerr << "\n";
            }
            
            // Debug: Show data segment contents
            const auto& dataSegment = semantic.getSymbolTable().dataSegment;
            std::cerr << "[INFO] Data segment: " << dataSegment.values.size() << " values\n";
            if (!dataSegment.values.empty()) {
                std::cerr << "[INFO] DATA values: ";
                for (size_t i = 0; i < dataSegment.values.size() && i < 10; ++i) {
                    if (i > 0) std::cerr << ", ";
                    std::cerr << "\"" << dataSegment.values[i] << "\"";
                }
                if (dataSegment.values.size() > 10) {
                    std::cerr << " ... (" << (dataSegment.values.size() - 10) << " more)";
                }
                std::cerr << "\n";
            }
        }
        
        // Dump the CFGs only if trace flag is enabled
        if (g_traceCFG) {
            std::cerr << "\n╔══════════════════════════════════════════════════════════════════════════╗\n";
            std::cerr << "║                    PROGRAM CFG ANALYSIS REPORT                           ║\n";
            std::cerr << "╚══════════════════════════════════════════════════════════════════════════╝\n\n";
            
            std::cerr << "Total CFGs: " << (1 + programCFG->functionCFGs.size()) << "\n";
            std::cerr << "  - Main Program: 1\n";
            std::cerr << "  - Functions/Subs: " << programCFG->functionCFGs.size() << "\n\n";
            
            // Dump main CFG with comprehensive analysis
            CFGBuilder mainBuilder;
            mainBuilder.setCFGForDump(programCFG->mainCFG.get());
            mainBuilder.dumpCFG("Main Program");
            mainBuilder.setCFGForDump(nullptr); // Clear to prevent deletion
            
            // Dump function/SUB CFGs with comprehensive analysis
            for (const auto& [name, cfg] : programCFG->functionCFGs) {
                CFGBuilder funcBuilder;
                funcBuilder.setCFGForDump(cfg.get());
                funcBuilder.dumpCFG(name);
                funcBuilder.setCFGForDump(nullptr); // Clear to prevent deletion
            }
        }
        
        // Generate QBE IL using new code generator v2
        if (g_showIL) {
            std::cerr << "\n========================================\n";
            std::cerr << "CODE GENERATION: V2 (CFG-aware)\n";
            std::cerr << "========================================\n\n";
        }
        
        fbc::QBECodeGeneratorV2 codegen(semantic);
        codegen.setDataValues(dataResult);  // Pass DATA values to code generator
        std::string qbeIL = codegen.generateProgram(ast.get(), programCFG);
        
        delete programCFG;
        
        if (qbeIL.empty()) {
            std::cerr << "[ERROR] Code generation produced empty IL\n";
            return nullptr;
        }
        
        if (g_showIL) {
            std::cerr << "[INFO] QBE IL generation successful (" << qbeIL.size() << " bytes)\n";
            std::cerr << "\n=== GENERATED QBE IL ===\n";
            std::cerr << qbeIL;
            std::cerr << "\n=== END QBE IL ===\n\n";
        }
        
        // Allocate and copy the result
        char *result = (char*)malloc(qbeIL.size() + 1);
        if (!result) {
            std::cerr << "[ERROR] Failed to allocate memory for IL\n";
            return nullptr;
        }
        std::strcpy(result, qbeIL.c_str());
        return result;
        
    } catch (const std::exception& e) {
        std::cerr << "FasterBASIC error: " << e.what() << "\n";
        return nullptr;
    } catch (...) {
        std::cerr << "FasterBASIC unknown error\n";
        return nullptr;
    }
}

/* Enable/disable CFG tracing */
void set_trace_cfg_impl(int enable) {
    g_traceCFG = (enable != 0);
}

/* Enable/disable AST tracing */
void set_trace_ast_impl(int enable) {
    g_traceAST = (enable != 0);
}

void set_trace_symbols_impl(int enable) {
    g_traceSymbols = (enable != 0);
}

/* Enable/disable IL output */
void set_show_il_impl(int enable) {
    g_showIL = (enable != 0);
    if (enable) {
        g_verbose = true;  // IL output implies verbose
    }
}

/* Enable/disable verbose output */
void set_verbose_impl(int enable) {
    g_verbose = (enable != 0);
}

} // extern "C"
