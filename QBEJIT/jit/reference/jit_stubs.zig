//
// jit_stubs.zig
// FasterBASIC JIT — Runtime Jump Table (Real Runtime)
//
// The real runtime (runtime/*.zig + runtime/basic_runtime.c) is linked
// directly into the fbc binary.  This module declares `extern fn` for
// every runtime symbol that JIT-generated code may call, and builds a
// jump table mapping the QBE external-call names (e.g. "_basic_print_int")
// to the real function addresses — resolved at link time.
//
// No stubs.  No dlsym.  Every entry points to the real implementation.
//

const std = @import("std");
const linker = @import("jit_linker.zig");

const JumpTableEntry = linker.JumpTableEntry;
const RuntimeContext = linker.RuntimeContext;

// ============================================================================
// Section: Extern Declarations — Real Runtime Functions
//
// These are resolved by the linker against the static libraries that are
// linked into the fbc binary (see build.zig).  The signatures don't need
// to be exact for address-taking purposes — we only need the symbol.
// We declare them with minimal signatures; the JIT machine code already
// has the correct calling convention baked in.
// ============================================================================

// ── Lifecycle ──
extern fn basic_init_args(...) callconv(.c) void;
extern fn basic_runtime_init() callconv(.c) void;
extern fn basic_runtime_cleanup() callconv(.c) void;
extern fn basic_exit(code: c_int) callconv(.c) noreturn;
extern fn basic_jit_call(callback: *const fn (*anyopaque) callconv(.c) c_int, ctx: *anyopaque) callconv(.c) c_int;
extern fn basic_jit_exec(fn_ptr: *const anyopaque, argc: c_int, argv: [*][*:0]u8) callconv(.c) c_int;
extern fn samm_init() callconv(.c) void;
extern fn samm_shutdown() callconv(.c) void;
extern fn samm_enter_scope() callconv(.c) void;
extern fn samm_exit_scope() callconv(.c) void;
extern fn samm_retain(...) callconv(.c) void;
extern fn samm_register_cleanup(...) callconv(.c) void;

// ── I/O ──
extern fn basic_print_int(...) callconv(.c) void;
extern fn basic_print_double(...) callconv(.c) void;
extern fn basic_print_string_desc(...) callconv(.c) void;
extern fn basic_print_newline() callconv(.c) void;
extern fn basic_print_tab() callconv(.c) void;
extern fn basic_print_lock() callconv(.c) void;
extern fn basic_print_unlock() callconv(.c) void;
extern fn basic_input_string() callconv(.c) ?*anyopaque;
extern fn basic_input_int() callconv(.c) i32;
extern fn basic_input_double() callconv(.c) f64;

// ── Strings ──
extern fn string_new_utf8(...) callconv(.c) ?*anyopaque;
extern fn string_concat(...) callconv(.c) ?*anyopaque;
extern fn string_compare(...) callconv(.c) i32;
extern fn string_length(...) callconv(.c) i64;
extern fn string_retain(...) callconv(.c) ?*anyopaque;
extern fn string_release(...) callconv(.c) void;
extern fn string_from_int(...) callconv(.c) ?*anyopaque;
extern fn string_from_double(...) callconv(.c) ?*anyopaque;
extern fn string_clone(...) callconv(.c) ?*anyopaque;
extern fn string_to_int(...) callconv(.c) i64;
extern fn string_to_double(...) callconv(.c) f64;
extern fn string_mid(...) callconv(.c) ?*anyopaque;
extern fn string_upper(...) callconv(.c) ?*anyopaque;
extern fn string_lower(...) callconv(.c) ?*anyopaque;
extern fn string_instr(...) callconv(.c) i64;
extern fn string_ltrim(...) callconv(.c) ?*anyopaque;
extern fn string_rtrim(...) callconv(.c) ?*anyopaque;
extern fn string_trim(...) callconv(.c) ?*anyopaque;
extern fn string_to_utf8(...) callconv(.c) [*:0]const u8;
extern fn basic_mid(...) callconv(.c) ?*anyopaque;
extern fn basic_left(...) callconv(.c) ?*anyopaque;
extern fn basic_right(...) callconv(.c) ?*anyopaque;
extern fn basic_chr(...) callconv(.c) ?*anyopaque;
extern fn basic_asc(...) callconv(.c) u32;
extern fn basic_string_repeat(...) callconv(.c) ?*anyopaque;
extern fn basic_space(...) callconv(.c) ?*anyopaque;
extern fn basic_val(...) callconv(.c) f64;
extern fn basic_len(...) callconv(.c) i64;
extern fn HEX_STRING(...) callconv(.c) ?*anyopaque;
extern fn OCT_STRING(...) callconv(.c) ?*anyopaque;
extern fn BIN_STRING(...) callconv(.c) ?*anyopaque;

// ── Math ──
extern fn basic_abs_int(...) callconv(.c) i32;
extern fn basic_sgn(...) callconv(.c) i32;
extern fn basic_rnd() callconv(.c) f64;
extern fn math_cint(...) callconv(.c) i32;

// ── Memory ──
extern fn basic_malloc(...) callconv(.c) ?*anyopaque;
extern fn basic_free(...) callconv(.c) void;

// ── Arrays ──
extern fn fbc_array_create(...) callconv(.c) void;
extern fn fbc_array_bounds_check(...) callconv(.c) void;
extern fn fbc_array_element_addr(...) callconv(.c) ?*anyopaque;
extern fn array_descriptor_erase(...) callconv(.c) void;

// ── 2D Arrays ──
extern fn fbc_array_create_2d(...) callconv(.c) void;
extern fn fbc_array_bounds_check_2d(...) callconv(.c) void;
extern fn fbc_array_element_addr_2d(...) callconv(.c) ?*anyopaque;

// ── Error / Debug ──
extern fn basic_error(...) callconv(.c) void;
extern fn basic_set_line(...) callconv(.c) void;

// ── Class / Object ──
extern fn class_object_new(...) callconv(.c) ?*anyopaque;
extern fn class_object_delete(...) callconv(.c) void;
extern fn class_is_instance(...) callconv(.c) i32;

// ── DATA / READ / RESTORE ──
extern fn basic_data_init(...) callconv(.c) void;
extern fn basic_read_data_string() callconv(.c) ?*anyopaque;
extern fn basic_read_data_int() callconv(.c) i32;
extern fn basic_read_data_double() callconv(.c) f64;
extern fn basic_restore_data() callconv(.c) void;

// ── Timer / Sleep ──
extern fn basic_timer() callconv(.c) f64;
extern fn basic_timer_ms() callconv(.c) i64;
extern fn basic_sleep_ms(...) callconv(.c) void;

// ── Hashmap ──
extern fn hashmap_new(...) callconv(.c) ?*anyopaque;
extern fn hashmap_insert(...) callconv(.c) void;
extern fn hashmap_lookup(...) callconv(.c) ?*anyopaque;
extern fn hashmap_has_key(...) callconv(.c) i32;
extern fn hashmap_remove(...) callconv(.c) void;
extern fn hashmap_size(...) callconv(.c) i64;
extern fn hashmap_clear(...) callconv(.c) void;
extern fn hashmap_keys(...) callconv(.c) ?*anyopaque;
extern fn hashmap_free(...) callconv(.c) void;

// ── List ──
extern fn list_create() callconv(.c) ?*anyopaque;
extern fn list_create_typed(...) callconv(.c) ?*anyopaque;
extern fn list_free(...) callconv(.c) void;
extern fn list_length(...) callconv(.c) i64;
extern fn list_empty(...) callconv(.c) i32;
extern fn list_append_int(...) callconv(.c) void;
extern fn list_append_float(...) callconv(.c) void;
extern fn list_append_string(...) callconv(.c) void;
extern fn list_append_object(...) callconv(.c) void;
extern fn list_append_list(...) callconv(.c) void;
extern fn list_prepend_int(...) callconv(.c) void;
extern fn list_prepend_float(...) callconv(.c) void;
extern fn list_prepend_string(...) callconv(.c) void;
extern fn list_prepend_list(...) callconv(.c) void;
extern fn list_insert_int(...) callconv(.c) void;
extern fn list_insert_float(...) callconv(.c) void;
extern fn list_insert_string(...) callconv(.c) void;
extern fn list_extend(...) callconv(.c) void;
extern fn list_head_int(...) callconv(.c) i64;
extern fn list_head_float(...) callconv(.c) f64;
extern fn list_head_ptr(...) callconv(.c) ?*anyopaque;
extern fn list_head_type(...) callconv(.c) i32;
extern fn list_get_int(...) callconv(.c) i64;
extern fn list_get_float(...) callconv(.c) f64;
extern fn list_get_ptr(...) callconv(.c) ?*anyopaque;
extern fn list_get_type(...) callconv(.c) i32;
extern fn list_pop_int(...) callconv(.c) i64;
extern fn list_pop_float(...) callconv(.c) f64;
extern fn list_pop_ptr(...) callconv(.c) ?*anyopaque;
extern fn list_pop(...) callconv(.c) void;
extern fn list_shift_int(...) callconv(.c) i64;
extern fn list_shift_float(...) callconv(.c) f64;
extern fn list_shift_ptr(...) callconv(.c) ?*anyopaque;
extern fn list_shift_type(...) callconv(.c) i32;
extern fn list_shift(...) callconv(.c) void;
extern fn list_remove(...) callconv(.c) void;
extern fn list_clear(...) callconv(.c) void;
extern fn list_copy(...) callconv(.c) ?*anyopaque;
extern fn list_rest(...) callconv(.c) ?*anyopaque;
extern fn list_reverse(...) callconv(.c) void;
extern fn list_erase(...) callconv(.c) void;
extern fn list_join(...) callconv(.c) ?*anyopaque;
extern fn list_contains_int(...) callconv(.c) i32;
extern fn list_contains_float(...) callconv(.c) i32;
extern fn list_contains_string(...) callconv(.c) i32;
extern fn list_indexof_int(...) callconv(.c) i64;
extern fn list_indexof_float(...) callconv(.c) i64;
extern fn list_indexof_string(...) callconv(.c) i64;
extern fn list_iter_begin(...) callconv(.c) ?*anyopaque;
extern fn list_iter_next(...) callconv(.c) ?*anyopaque;
extern fn list_iter_type(...) callconv(.c) i32;
extern fn list_iter_value_int(...) callconv(.c) i64;
extern fn list_iter_value_float(...) callconv(.c) f64;
extern fn list_iter_value_ptr(...) callconv(.c) ?*anyopaque;
extern fn list_debug_print(...) callconv(.c) void;
extern fn list_free_from_samm(...) callconv(.c) void;
extern fn list_atom_free_from_samm(...) callconv(.c) void;

// ── Terminal I/O ──
extern fn terminal_init() callconv(.c) void;
extern fn terminal_cleanup() callconv(.c) void;
extern fn terminal_flush() callconv(.c) void;
extern fn basic_locate(...) callconv(.c) void;
extern fn basic_cls() callconv(.c) void;
extern fn basic_gcls() callconv(.c) void;
extern fn basic_clear_eol() callconv(.c) void;
extern fn basic_clear_eos() callconv(.c) void;
extern fn basic_wrch(...) callconv(.c) void;
extern fn basic_wrstr(...) callconv(.c) void;
extern fn hideCursor() callconv(.c) void;
extern fn showCursor() callconv(.c) void;
extern fn saveCursor() callconv(.c) void;
extern fn restoreCursor() callconv(.c) void;
extern fn cursorUp(...) callconv(.c) void;
extern fn cursorDown(...) callconv(.c) void;
extern fn cursorLeft(...) callconv(.c) void;
extern fn cursorRight(...) callconv(.c) void;
extern fn basic_color(...) callconv(.c) void;
extern fn basic_color_bg(...) callconv(.c) void;
extern fn basic_color_rgb(...) callconv(.c) void;
extern fn basic_color_rgb_bg(...) callconv(.c) void;
extern fn basic_color_reset() callconv(.c) void;
extern fn basic_style_bold() callconv(.c) void;
extern fn basic_style_dim() callconv(.c) void;
extern fn basic_style_italic() callconv(.c) void;
extern fn basic_style_underline() callconv(.c) void;
extern fn basic_style_blink() callconv(.c) void;
extern fn basic_style_reverse() callconv(.c) void;
extern fn basic_style_reset() callconv(.c) void;
extern fn basic_screen_alternate() callconv(.c) void;
extern fn basic_screen_main() callconv(.c) void;
extern fn basic_get_cursor_pos(...) callconv(.c) void;
extern fn basic_flush() callconv(.c) void;
extern fn basic_begin_draw() callconv(.c) void;
extern fn basic_end_draw() callconv(.c) void;
extern fn terminal_get_width() callconv(.c) i32;
extern fn terminal_get_height() callconv(.c) i32;
extern fn basic_kbraw() callconv(.c) void;
extern fn basic_kbecho() callconv(.c) void;
extern fn basic_kbhit() callconv(.c) i32;
extern fn basic_kbget() callconv(.c) i32;
extern fn basic_kbpeek() callconv(.c) i32;
extern fn basic_kbcode() callconv(.c) i32;
extern fn basic_kbspecial() callconv(.c) i32;
extern fn basic_kbmod() callconv(.c) i32;
extern fn basic_kbflush() callconv(.c) void;
extern fn basic_kbclear() callconv(.c) void;
extern fn basic_kbcount() callconv(.c) i32;
extern fn basic_inkey() callconv(.c) ?*anyopaque;
extern fn basic_pos() callconv(.c) i32;
extern fn basic_row() callconv(.c) i32;
extern fn basic_csrlin() callconv(.c) i32;
extern fn basic_mouse_enable() callconv(.c) void;
extern fn basic_mouse_disable() callconv(.c) void;
extern fn basic_mouse_x() callconv(.c) i32;
extern fn basic_mouse_y() callconv(.c) i32;
extern fn basic_mouse_buttons() callconv(.c) i32;
extern fn basic_mouse_button(...) callconv(.c) i32;
extern fn basic_mouse_poll() callconv(.c) void;

// ── Worker / Parallel ──
extern fn worker_spawn(...) callconv(.c) ?*anyopaque;
extern fn worker_spawn_messaging(...) callconv(.c) ?*anyopaque;
extern fn worker_await(...) callconv(.c) f64;
extern fn worker_ready(...) callconv(.c) i32;
extern fn worker_args_alloc(...) callconv(.c) ?*anyopaque;
extern fn worker_args_set_double(...) callconv(.c) void;
extern fn worker_args_set_int(...) callconv(.c) void;
extern fn worker_args_set_ptr(...) callconv(.c) void;
extern fn worker_future_outbox_offset() callconv(.c) i32;
extern fn worker_future_inbox_offset() callconv(.c) i32;

// ── Worker Messaging ──
extern fn msg_queue_create() callconv(.c) ?*anyopaque;
extern fn msg_queue_destroy(...) callconv(.c) void;
extern fn msg_queue_push(...) callconv(.c) i32;
extern fn msg_queue_pop(...) callconv(.c) ?*anyopaque;
extern fn msg_queue_has_message(...) callconv(.c) i32;
extern fn msg_queue_close(...) callconv(.c) void;
extern fn msg_send_double(...) callconv(.c) i32;
extern fn msg_send_int(...) callconv(.c) i32;
extern fn msg_send_string(...) callconv(.c) i32;
extern fn msg_send_udt(...) callconv(.c) i32;
extern fn msg_send_marshalled(...) callconv(.c) i32;
extern fn msg_send_udt_typed(...) callconv(.c) i32;
extern fn msg_send_class(...) callconv(.c) i32;
extern fn msg_receive_double(...) callconv(.c) f64;
extern fn msg_receive_int(...) callconv(.c) i32;
extern fn msg_receive_string(...) callconv(.c) ?*anyopaque;
extern fn msg_receive_udt(...) callconv(.c) void;
extern fn msg_receive_marshalled(...) callconv(.c) ?*anyopaque;
extern fn msg_cancel(...) callconv(.c) void;
extern fn msg_is_cancelled(...) callconv(.c) i32;
extern fn msg_get_outbox(...) callconv(.c) ?*anyopaque;
extern fn msg_get_inbox(...) callconv(.c) ?*anyopaque;
extern fn msg_drain_and_destroy(...) callconv(.c) void;
extern fn msg_marshall_double(...) callconv(.c) ?*anyopaque;
extern fn msg_marshall_int(...) callconv(.c) ?*anyopaque;
extern fn msg_marshall_signal(...) callconv(.c) ?*anyopaque;
extern fn msg_marshall_udt_typed(...) callconv(.c) ?*anyopaque;
extern fn msg_marshall_class(...) callconv(.c) ?*anyopaque;
extern fn msg_marshall_string(...) callconv(.c) ?*anyopaque;
extern fn msg_marshall_array(...) callconv(.c) ?*anyopaque;
extern fn msg_blob_free(...) callconv(.c) void;
extern fn msg_blob_tag(...) callconv(.c) i32;
extern fn msg_blob_type_id(...) callconv(.c) i32;
extern fn msg_blob_payload_ptr(...) callconv(.c) ?*anyopaque;
extern fn msg_blob_forward(...) callconv(.c) i32;
extern fn msg_unmarshall_double(...) callconv(.c) f64;
extern fn msg_unmarshall_int(...) callconv(.c) i32;
extern fn msg_unmarshall_string(...) callconv(.c) ?*anyopaque;
extern fn msg_unmarshall_udt(...) callconv(.c) void;
extern fn msg_unmarshall_array(...) callconv(.c) void;

// ── Marshalling ──
extern fn marshall_array(...) callconv(.c) ?*anyopaque;
extern fn marshall_udt(...) callconv(.c) ?*anyopaque;
extern fn marshall_udt_deep(...) callconv(.c) ?*anyopaque;
extern fn unmarshall_array(...) callconv(.c) void;
extern fn unmarshall_udt(...) callconv(.c) void;
extern fn unmarshall_udt_deep(...) callconv(.c) void;

// ── Additional I/O ──
extern fn basic_print_long(...) callconv(.c) void;
extern fn basic_print_float(...) callconv(.c) void;
extern fn basic_print_string(...) callconv(.c) void;
extern fn basic_print_cstr(...) callconv(.c) void;
extern fn basic_print_hex(...) callconv(.c) void;
extern fn basic_print_pointer(...) callconv(.c) void;
extern fn basic_print_at(...) callconv(.c) void;
extern fn basic_input_prompt(...) callconv(.c) ?*anyopaque;
extern fn basic_input_line() callconv(.c) ?*anyopaque;
extern fn basic_line_input() callconv(.c) ?*anyopaque;
extern fn basic_eof(...) callconv(.c) i32;
extern fn file_open(...) callconv(.c) ?*anyopaque;
extern fn file_close(...) callconv(.c) void;
extern fn file_print_string(...) callconv(.c) void;
extern fn file_print_int(...) callconv(.c) void;
extern fn file_print_newline(...) callconv(.c) void;
extern fn file_print_double(...) callconv(.c) void;
extern fn file_read_line(...) callconv(.c) ?*anyopaque;
extern fn file_eof(...) callconv(.c) i32;
extern fn file_get_handle(...) callconv(.c) ?*anyopaque;
extern fn file_set_handle(...) callconv(.c) void;
extern fn basic_system(...) callconv(.c) void;
extern fn basic_shell(...) callconv(.c) void;
extern fn basic_slurp(...) callconv(.c) ?*anyopaque;
extern fn basic_spit(...) callconv(.c) void;

// ── Additional string ops ──
extern fn string_left(...) callconv(.c) ?*anyopaque;
extern fn string_right(...) callconv(.c) ?*anyopaque;
extern fn string_replace(...) callconv(.c) ?*anyopaque;
extern fn string_reverse(...) callconv(.c) ?*anyopaque;
extern fn string_tally(...) callconv(.c) i64;
extern fn string_instrrev(...) callconv(.c) i64;
extern fn string_insert(...) callconv(.c) ?*anyopaque;
extern fn string_delete(...) callconv(.c) ?*anyopaque;
extern fn string_remove(...) callconv(.c) ?*anyopaque;
extern fn string_extract(...) callconv(.c) ?*anyopaque;
extern fn string_lpad(...) callconv(.c) ?*anyopaque;
extern fn string_rpad(...) callconv(.c) ?*anyopaque;
extern fn string_center(...) callconv(.c) ?*anyopaque;
extern fn string_space(...) callconv(.c) ?*anyopaque;
extern fn string_repeat(...) callconv(.c) ?*anyopaque;
extern fn string_join(...) callconv(.c) ?*anyopaque;
extern fn string_split(...) callconv(.c) ?*anyopaque;
extern fn string_new_ascii(...) callconv(.c) ?*anyopaque;
extern fn string_new_capacity(...) callconv(.c) ?*anyopaque;
extern fn string_new_utf32(...) callconv(.c) ?*anyopaque;

// ── Conversion ──
extern fn int_to_str(...) callconv(.c) ?*anyopaque;
extern fn long_to_str(...) callconv(.c) ?*anyopaque;
extern fn float_to_str(...) callconv(.c) ?*anyopaque;
extern fn double_to_str(...) callconv(.c) ?*anyopaque;
extern fn str_to_int(...) callconv(.c) i32;
extern fn str_to_long(...) callconv(.c) i64;
extern fn str_to_float(...) callconv(.c) f32;
extern fn str_to_double(...) callconv(.c) f64;

// ── Additional math ──
extern fn basic_abs_double(...) callconv(.c) f64;
extern fn basic_sqrt(...) callconv(.c) f64;

// ── C libm bare-name stubs for JIT alias table ──
// Zig names avoid clashing with builtins; they link against the
// C library symbols via @export / callconv(.c).
fn libm_sinh(x: f64) callconv(.c) f64 {
    return @call(.auto, std.math.sinh, .{x});
}
fn libm_cosh(x: f64) callconv(.c) f64 {
    return @call(.auto, std.math.cosh, .{x});
}
fn libm_tanh(x: f64) callconv(.c) f64 {
    return @call(.auto, std.math.tanh, .{x});
}
fn libm_hypot(x: f64, y: f64) callconv(.c) f64 {
    return @call(.auto, std.math.hypot, .{ x, y });
}
fn libm_cbrt(x: f64) callconv(.c) f64 {
    return @call(.auto, std.math.cbrt, .{x});
}
extern fn basic_pow(...) callconv(.c) f64;
extern fn basic_sin(...) callconv(.c) f64;
extern fn basic_cos(...) callconv(.c) f64;
extern fn basic_tan(...) callconv(.c) f64;
extern fn basic_asin(...) callconv(.c) f64;
extern fn basic_acos(...) callconv(.c) f64;
extern fn basic_atan(...) callconv(.c) f64;
extern fn basic_atan2(...) callconv(.c) f64;
extern fn basic_log(...) callconv(.c) f64;
extern fn basic_log10(...) callconv(.c) f64;
extern fn basic_exp(...) callconv(.c) f64;
extern fn basic_floor(...) callconv(.c) f64;
extern fn basic_ceil(...) callconv(.c) f64;
extern fn basic_int(...) callconv(.c) i32;
extern fn basic_fix(...) callconv(.c) i32;
extern fn basic_rand(...) callconv(.c) i32;
extern fn basic_rnd_int(...) callconv(.c) i32;
extern fn basic_randomize(...) callconv(.c) void;
extern fn basic_round(...) callconv(.c) f64;

// ── Error / exception ──
extern fn basic_error_msg(...) callconv(.c) void;
extern fn basic_throw(...) callconv(.c) void;
extern fn basic_exception_push(...) callconv(.c) ?*anyopaque;
extern fn basic_exception_pop() callconv(.c) void;
extern fn basic_err() callconv(.c) i32;
extern fn basic_erl() callconv(.c) i32;
extern fn basic_setjmp() callconv(.c) i32;

// ── Globals ──
extern fn basic_global_init(...) callconv(.c) void;
extern fn basic_global_base() callconv(.c) ?*anyopaque;
extern fn basic_global_cleanup() callconv(.c) void;

// ── Array operations (array_ops.zig) ──
extern fn fbc_array_redim(...) callconv(.c) void;
extern fn fbc_array_redim_preserve(...) callconv(.c) void;
extern fn fbc_array_erase(...) callconv(.c) void;
extern fn fbc_array_lbound(...) callconv(.c) i32;
extern fn fbc_array_ubound(...) callconv(.c) i32;

// ── Binary I/O ──
extern fn file_put_record(...) callconv(.c) void;
extern fn file_get_record(...) callconv(.c) void;
extern fn file_seek(...) callconv(.c) i32;
extern fn basic_loc(...) callconv(.c) c_long;
extern fn basic_lof(...) callconv(.c) c_long;

// ── MK$/CV$ binary conversion ──
extern fn basic_mki(...) callconv(.c) ?*anyopaque;
extern fn basic_mks(...) callconv(.c) ?*anyopaque;
extern fn basic_mkd(...) callconv(.c) ?*anyopaque;
extern fn basic_cvi(...) callconv(.c) i32;
extern fn basic_cvs(...) callconv(.c) f64;
extern fn basic_cvd(...) callconv(.c) f64;

// ── Command-line arguments ──
extern fn basic_command_count() callconv(.c) i32;
extern fn basic_command(...) callconv(.c) ?*anyopaque;

// ── String ops (legacy BasicString) ──
extern fn str_new(...) callconv(.c) ?*anyopaque;
extern fn str_retain(...) callconv(.c) ?*anyopaque;
extern fn str_release(...) callconv(.c) void;
extern fn str_concat(...) callconv(.c) ?*anyopaque;
extern fn str_compare(...) callconv(.c) i32;
extern fn str_length(...) callconv(.c) i32;

// ── SAMM extended ──
extern fn samm_track(...) callconv(.c) void;
extern fn samm_track_object(...) callconv(.c) void;
extern fn samm_untrack(...) callconv(.c) void;
extern fn samm_alloc_object(...) callconv(.c) ?*anyopaque;
extern fn samm_free_object(...) callconv(.c) void;
extern fn samm_alloc_string() callconv(.c) ?*anyopaque;
extern fn samm_track_string(...) callconv(.c) void;
extern fn samm_alloc_list() callconv(.c) ?*anyopaque;
extern fn samm_track_list(...) callconv(.c) void;
extern fn samm_alloc_list_atom() callconv(.c) ?*anyopaque;
extern fn samm_is_enabled() callconv(.c) i32;
extern fn samm_scope_depth() callconv(.c) i32;
extern fn samm_print_stats() callconv(.c) void;

// ── Timer SEND ──
extern fn timer_after_send(...) callconv(.c) i32;
extern fn timer_every_send(...) callconv(.c) i32;
extern fn timer_stop(...) callconv(.c) void;
extern fn timer_stop_all() callconv(.c) void;

// ============================================================================
// Section: Jump Table Construction
// ============================================================================

/// Convert any extern function reference to a u64 address at runtime.
fn fnAddr(comptime func: anytype) u64 {
    return @intFromPtr(&func);
}

/// Comptime-known symbol names for the jump table.
/// The names use the leading-underscore convention that QBE emits in
/// CALL_EXT instructions (matching the macOS Mach-O C symbol prefix).
const entry_names = [_][]const u8{
    // ── Lifecycle (0-8) ──
    "_basic_init_args",
    "_basic_runtime_init",
    "_basic_runtime_cleanup",
    "_basic_exit",
    "_basic_jit_call",
    "_basic_jit_exec",
    "_samm_init",
    "_samm_shutdown",
    "_samm_enter_scope",
    "_samm_exit_scope",
    "_samm_retain",
    "_samm_register_cleanup",

    // ── I/O (9-17) ──
    "_basic_print_int",
    "_basic_print_double",
    "_basic_print_string_desc",
    "_basic_print_newline",
    "_basic_print_tab",
    "_basic_print_lock",
    "_basic_print_unlock",
    "_basic_input_string",
    "_basic_input_int",
    "_basic_input_double",

    // ── Strings (18-50) ──
    "_string_new_utf8",
    "_string_concat",
    "_string_compare",
    "_string_length",
    "_string_retain",
    "_string_release",
    "_string_from_int",
    "_string_from_double",
    "_string_clone",
    "_string_to_int",
    "_string_to_double",
    "_string_mid",
    "_string_upper",
    "_string_lower",
    "_string_instr",
    "_string_ltrim",
    "_string_rtrim",
    "_string_trim",
    "_string_to_utf8",
    "_basic_mid",
    "_basic_left",
    "_basic_right",
    "_basic_chr",
    "_basic_asc",
    "_basic_string_repeat",
    "_basic_space",
    "_basic_val",
    "_basic_len",
    "_HEX_STRING",
    "_OCT_STRING",
    "_BIN_STRING",

    // ── Math (51-54) ──
    "_basic_abs_int",
    "_basic_sgn",
    "_basic_rnd",
    "_math_cint",

    // ── Memory (55-56) ──
    "_basic_malloc",
    "_basic_free",

    // ── Arrays (57-60) ──
    "_fbc_array_create",
    "_fbc_array_bounds_check",
    "_fbc_array_element_addr",
    "_array_descriptor_erase",

    // ── 2D Arrays (61-63) ──
    "_fbc_array_create_2d",
    "_fbc_array_bounds_check_2d",
    "_fbc_array_element_addr_2d",

    // ── Error / Debug (64-65) ──
    "_basic_error",
    "_basic_set_line",

    // ── Class / Object (63-65) ──
    "_class_object_new",
    "_class_object_delete",
    "_class_is_instance",

    // ── DATA / READ / RESTORE (66-70) ──
    "_basic_data_init",
    "_basic_read_data_string",
    "_basic_read_data_int",
    "_basic_read_data_double",
    "_basic_restore_data",

    // ── Timer / Sleep (71-73) ──
    "_basic_timer",
    "_basic_timer_ms",
    "_basic_sleep_ms",

    // ── Hashmap (74-82) ──
    "_hashmap_new",
    "_hashmap_insert",
    "_hashmap_lookup",
    "_hashmap_has_key",
    "_hashmap_remove",
    "_hashmap_size",
    "_hashmap_clear",
    "_hashmap_keys",
    "_hashmap_free",

    // ── List ──
    "_list_create",
    "_list_create_typed",
    "_list_free",
    "_list_length",
    "_list_empty",
    "_list_append_int",
    "_list_append_float",
    "_list_append_string",
    "_list_append_object",
    "_list_append_list",
    "_list_prepend_int",
    "_list_prepend_float",
    "_list_prepend_string",
    "_list_prepend_list",
    "_list_insert_int",
    "_list_insert_float",
    "_list_insert_string",
    "_list_extend",
    "_list_head_int",
    "_list_head_float",
    "_list_head_ptr",
    "_list_head_type",
    "_list_get_int",
    "_list_get_float",
    "_list_get_ptr",
    "_list_get_type",
    "_list_pop_int",
    "_list_pop_float",
    "_list_pop_ptr",
    "_list_pop",
    "_list_shift_int",
    "_list_shift_float",
    "_list_shift_ptr",
    "_list_shift_type",
    "_list_shift",
    "_list_remove",
    "_list_clear",
    "_list_copy",
    "_list_rest",
    "_list_reverse",
    "_list_erase",
    "_list_join",
    "_list_contains_int",
    "_list_contains_float",
    "_list_contains_string",
    "_list_indexof_int",
    "_list_indexof_float",
    "_list_indexof_string",
    "_list_iter_begin",
    "_list_iter_next",
    "_list_iter_type",
    "_list_iter_value_int",
    "_list_iter_value_float",
    "_list_iter_value_ptr",
    "_list_debug_print",
    "_list_free_from_samm",
    "_list_atom_free_from_samm",

    // ── Terminal I/O (97-) ──
    "_terminal_init",
    "_terminal_cleanup",
    "_terminal_flush",
    "_basic_locate",
    "_basic_cls",
    "_basic_gcls",
    "_basic_clear_eol",
    "_basic_clear_eos",
    "_basic_wrch",
    "_basic_wrstr",
    "_hideCursor",
    "_showCursor",
    "_saveCursor",
    "_restoreCursor",
    "_cursorUp",
    "_cursorDown",
    "_cursorLeft",
    "_cursorRight",
    "_basic_color",
    "_basic_color_bg",
    "_basic_color_rgb",
    "_basic_color_rgb_bg",
    "_basic_color_reset",
    "_basic_style_bold",
    "_basic_style_dim",
    "_basic_style_italic",
    "_basic_style_underline",
    "_basic_style_blink",
    "_basic_style_reverse",
    "_basic_style_reset",
    "_basic_screen_alternate",
    "_basic_screen_main",
    "_basic_get_cursor_pos",
    "_basic_flush",
    "_basic_begin_draw",
    "_basic_end_draw",
    "_terminal_get_width",
    "_terminal_get_height",
    "_basic_kbraw",
    "_basic_kbecho",
    "_basic_kbhit",
    "_basic_kbget",
    "_basic_kbpeek",
    "_basic_kbcode",
    "_basic_kbspecial",
    "_basic_kbmod",
    "_basic_kbflush",
    "_basic_kbclear",
    "_basic_kbcount",
    "_basic_inkey",
    "_basic_pos",
    "_basic_row",
    "_basic_csrlin",
    "_basic_mouse_enable",
    "_basic_mouse_disable",
    "_basic_mouse_x",
    "_basic_mouse_y",
    "_basic_mouse_buttons",
    "_basic_mouse_button",
    "_basic_mouse_poll",

    // ── Worker / Parallel ──
    "_worker_spawn",
    "_worker_spawn_messaging",
    "_worker_await",
    "_worker_ready",
    "_worker_args_alloc",
    "_worker_args_set_double",
    "_worker_args_set_int",
    "_worker_args_set_ptr",
    "_worker_future_outbox_offset",
    "_worker_future_inbox_offset",

    // ── Worker Messaging ──
    "_msg_queue_create",
    "_msg_queue_destroy",
    "_msg_queue_push",
    "_msg_queue_pop",
    "_msg_queue_has_message",
    "_msg_queue_close",
    "_msg_send_double",
    "_msg_send_int",
    "_msg_send_string",
    "_msg_send_udt",
    "_msg_send_marshalled",
    "_msg_send_udt_typed",
    "_msg_send_class",
    "_msg_receive_double",
    "_msg_receive_int",
    "_msg_receive_string",
    "_msg_receive_udt",
    "_msg_receive_marshalled",
    "_msg_cancel",
    "_msg_is_cancelled",
    "_msg_get_outbox",
    "_msg_get_inbox",
    "_msg_drain_and_destroy",
    "_msg_marshall_double",
    "_msg_marshall_int",
    "_msg_marshall_signal",
    "_msg_marshall_udt_typed",
    "_msg_marshall_class",
    "_msg_marshall_string",
    "_msg_marshall_array",
    "_msg_blob_free",
    "_msg_blob_tag",
    "_msg_blob_type_id",
    "_msg_blob_payload_ptr",
    "_msg_blob_forward",
    "_msg_unmarshall_double",
    "_msg_unmarshall_int",
    "_msg_unmarshall_string",
    "_msg_unmarshall_udt",
    "_msg_unmarshall_array",

    // ── Marshalling ──
    "_marshall_array",
    "_marshall_udt",
    "_marshall_udt_deep",
    "_unmarshall_array",
    "_unmarshall_udt",
    "_unmarshall_udt_deep",

    // ── Additional I/O ──
    "_basic_print_long",
    "_basic_print_float",
    "_basic_print_string",
    "_basic_print_cstr",
    "_basic_print_hex",
    "_basic_print_pointer",
    "_basic_print_at",
    "_basic_input_prompt",
    "_basic_input_line",
    "_basic_line_input",
    "_basic_eof",
    "_file_open",
    "_file_close",
    "_file_print_string",
    "_file_print_int",
    "_file_print_newline",
    "_file_print_double",
    "_file_read_line",
    "_file_eof",
    "_file_get_handle",
    "_file_set_handle",
    "_basic_system",
    "_basic_shell",
    "_basic_slurp",
    "_basic_spit",

    // ── String ops (legacy BasicString) ──
    "_str_new",
    "_str_retain",
    "_str_release",
    "_str_concat",
    "_str_compare",
    "_str_length",

    // ── Conversion ──
    "_int_to_str",
    "_long_to_str",
    "_float_to_str",
    "_double_to_str",
    "_str_to_int",
    "_str_to_long",
    "_str_to_float",
    "_str_to_double",

    // ── Extended string ops ──
    "_string_left",
    "_string_right",
    "_string_replace",
    "_string_reverse",
    "_string_new_ascii",
    "_string_new_capacity",
    "_string_space",
    "_string_repeat",

    // ── Additional math ──
    "_basic_abs_double",
    "_basic_sqrt",
    "_basic_pow",
    "_basic_sin",
    "_basic_cos",
    "_basic_tan",
    "_basic_asin",
    "_basic_acos",
    "_basic_atan",
    "_basic_atan2",
    "_basic_log",
    "_basic_log10",
    "_basic_exp",
    "_basic_floor",
    "_basic_ceil",
    "_basic_int",
    "_basic_fix",
    "_basic_rand",
    "_basic_rnd_int",
    "_basic_randomize",
    "_basic_round",

    // ── Bare C math aliases (QBE IL emits $sqrt, $pow, etc.) ──
    "_sqrt",
    "_pow",
    "_fabs",
    "_sin",
    "_cos",
    "_tan",
    "_asin",
    "_acos",
    "_atan",
    "_atan2",
    "_log",
    "_log10",
    "_exp",
    "_floor",
    "_ceil",
    "_round",
    "_trunc",
    "_sinh",
    "_cosh",
    "_tanh",
    "_hypot",
    "_cbrt",
    "_math_cint",

    // ── Error / exception ──
    "_basic_error_msg",
    "_basic_throw",
    "_basic_exception_push",
    "_basic_exception_pop",
    "_basic_err",
    "_basic_erl",
    "_basic_setjmp",

    // ── Globals ──
    "_basic_global_init",
    "_basic_global_base",
    "_basic_global_cleanup",

    // ── Array operations extended ──
    "_fbc_array_redim",
    "_fbc_array_redim_preserve",
    "_fbc_array_erase",
    "_fbc_array_lbound",
    "_fbc_array_ubound",

    // ── SAMM extended ──
    "_samm_track",
    "_samm_track_object",
    "_samm_untrack",
    "_samm_alloc_object",
    "_samm_free_object",
    "_samm_alloc_string",
    "_samm_track_string",
    "_samm_alloc_list",
    "_samm_track_list",
    "_samm_alloc_list_atom",
    "_samm_is_enabled",
    "_samm_scope_depth",
    "_samm_print_stats",

    // ── Timer SEND ──
    "_timer_after_send",
    "_timer_every_send",
    "_timer_stop",
    "_timer_stop_all",

    // ── Binary I/O ──
    "_file_put_record",
    "_file_get_record",
    "_file_seek",
    "_basic_loc",
    "_basic_lof",

    // ── MK$/CV$ binary conversion ──
    "_basic_mki",
    "_basic_mks",
    "_basic_mkd",
    "_basic_cvi",
    "_basic_cvs",
    "_basic_cvd",

    // ── Command-line arguments ──
    "_basic_command_count",
    "_basic_command",
};

/// Total number of entries in the jump table.
pub const NUM_ENTRIES: usize = entry_names.len;

/// Singleton storage for JumpTableEntry array — runtime-initialized
/// because extern fn addresses are not comptime-known.
var entries_storage: [entry_names.len]JumpTableEntry = undefined;
var entries_initialized: bool = false;

/// Populate entries_storage with names + real function addresses.
/// Called once on first use.  The address array is built here at
/// runtime so that `@intFromPtr(&extern_fn)` is evaluated lazily.
fn initEntries() void {
    const addrs = [entry_names.len]u64{
        // ── Lifecycle ──
        fnAddr(basic_init_args),
        fnAddr(basic_runtime_init),
        fnAddr(basic_runtime_cleanup),
        fnAddr(basic_exit),
        fnAddr(basic_jit_call),
        fnAddr(basic_jit_exec),
        fnAddr(samm_init),
        fnAddr(samm_shutdown),
        fnAddr(samm_enter_scope),
        fnAddr(samm_exit_scope),
        fnAddr(samm_retain),
        fnAddr(samm_register_cleanup),

        // ── I/O ──
        fnAddr(basic_print_int),
        fnAddr(basic_print_double),
        fnAddr(basic_print_string_desc),
        fnAddr(basic_print_newline),
        fnAddr(basic_print_tab),
        fnAddr(basic_print_lock),
        fnAddr(basic_print_unlock),
        fnAddr(basic_input_string),
        fnAddr(basic_input_int),
        fnAddr(basic_input_double),

        // ── Strings ──
        fnAddr(string_new_utf8),
        fnAddr(string_concat),
        fnAddr(string_compare),
        fnAddr(string_length),
        fnAddr(string_retain),
        fnAddr(string_release),
        fnAddr(string_from_int),
        fnAddr(string_from_double),
        fnAddr(string_clone),
        fnAddr(string_to_int),
        fnAddr(string_to_double),
        fnAddr(string_mid),
        fnAddr(string_upper),
        fnAddr(string_lower),
        fnAddr(string_instr),
        fnAddr(string_ltrim),
        fnAddr(string_rtrim),
        fnAddr(string_trim),
        fnAddr(string_to_utf8),
        fnAddr(basic_mid),
        fnAddr(basic_left),
        fnAddr(basic_right),
        fnAddr(basic_chr),
        fnAddr(basic_asc),
        fnAddr(basic_string_repeat),
        fnAddr(basic_space),
        fnAddr(basic_val),
        fnAddr(basic_len),
        fnAddr(HEX_STRING),
        fnAddr(OCT_STRING),
        fnAddr(BIN_STRING),

        // ── Math ──
        fnAddr(basic_abs_int),
        fnAddr(basic_sgn),
        fnAddr(basic_rnd),
        fnAddr(math_cint),

        // ── Memory ──
        fnAddr(basic_malloc),
        fnAddr(basic_free),

        // ── Arrays ──
        fnAddr(fbc_array_create),
        fnAddr(fbc_array_bounds_check),
        fnAddr(fbc_array_element_addr),
        fnAddr(array_descriptor_erase),

        // ── 2D Arrays ──
        fnAddr(fbc_array_create_2d),
        fnAddr(fbc_array_bounds_check_2d),
        fnAddr(fbc_array_element_addr_2d),

        // ── Error / Debug ──
        fnAddr(basic_error),
        fnAddr(basic_set_line),

        // ── Class / Object ──
        fnAddr(class_object_new),
        fnAddr(class_object_delete),
        fnAddr(class_is_instance),

        // ── DATA / READ / RESTORE ──
        fnAddr(basic_data_init),
        fnAddr(basic_read_data_string),
        fnAddr(basic_read_data_int),
        fnAddr(basic_read_data_double),
        fnAddr(basic_restore_data),

        // ── Timer / Sleep ──
        fnAddr(basic_timer),
        fnAddr(basic_timer_ms),
        fnAddr(basic_sleep_ms),

        // ── Hashmap ──
        fnAddr(hashmap_new),
        fnAddr(hashmap_insert),
        fnAddr(hashmap_lookup),
        fnAddr(hashmap_has_key),
        fnAddr(hashmap_remove),
        fnAddr(hashmap_size),
        fnAddr(hashmap_clear),
        fnAddr(hashmap_keys),
        fnAddr(hashmap_free),

        // ── List ──
        fnAddr(list_create),
        fnAddr(list_create_typed),
        fnAddr(list_free),
        fnAddr(list_length),
        fnAddr(list_empty),
        fnAddr(list_append_int),
        fnAddr(list_append_float),
        fnAddr(list_append_string),
        fnAddr(list_append_object),
        fnAddr(list_append_list),
        fnAddr(list_prepend_int),
        fnAddr(list_prepend_float),
        fnAddr(list_prepend_string),
        fnAddr(list_prepend_list),
        fnAddr(list_insert_int),
        fnAddr(list_insert_float),
        fnAddr(list_insert_string),
        fnAddr(list_extend),
        fnAddr(list_head_int),
        fnAddr(list_head_float),
        fnAddr(list_head_ptr),
        fnAddr(list_head_type),
        fnAddr(list_get_int),
        fnAddr(list_get_float),
        fnAddr(list_get_ptr),
        fnAddr(list_get_type),
        fnAddr(list_pop_int),
        fnAddr(list_pop_float),
        fnAddr(list_pop_ptr),
        fnAddr(list_pop),
        fnAddr(list_shift_int),
        fnAddr(list_shift_float),
        fnAddr(list_shift_ptr),
        fnAddr(list_shift_type),
        fnAddr(list_shift),
        fnAddr(list_remove),
        fnAddr(list_clear),
        fnAddr(list_copy),
        fnAddr(list_rest),
        fnAddr(list_reverse),
        fnAddr(list_erase),
        fnAddr(list_join),
        fnAddr(list_contains_int),
        fnAddr(list_contains_float),
        fnAddr(list_contains_string),
        fnAddr(list_indexof_int),
        fnAddr(list_indexof_float),
        fnAddr(list_indexof_string),
        fnAddr(list_iter_begin),
        fnAddr(list_iter_next),
        fnAddr(list_iter_type),
        fnAddr(list_iter_value_int),
        fnAddr(list_iter_value_float),
        fnAddr(list_iter_value_ptr),
        fnAddr(list_debug_print),
        fnAddr(list_free_from_samm),
        fnAddr(list_atom_free_from_samm),

        // ── Terminal I/O ──
        fnAddr(terminal_init),
        fnAddr(terminal_cleanup),
        fnAddr(terminal_flush),
        fnAddr(basic_locate),
        fnAddr(basic_cls),
        fnAddr(basic_gcls),
        fnAddr(basic_clear_eol),
        fnAddr(basic_clear_eos),
        fnAddr(basic_wrch),
        fnAddr(basic_wrstr),
        fnAddr(hideCursor),
        fnAddr(showCursor),
        fnAddr(saveCursor),
        fnAddr(restoreCursor),
        fnAddr(cursorUp),
        fnAddr(cursorDown),
        fnAddr(cursorLeft),
        fnAddr(cursorRight),
        fnAddr(basic_color),
        fnAddr(basic_color_bg),
        fnAddr(basic_color_rgb),
        fnAddr(basic_color_rgb_bg),
        fnAddr(basic_color_reset),
        fnAddr(basic_style_bold),
        fnAddr(basic_style_dim),
        fnAddr(basic_style_italic),
        fnAddr(basic_style_underline),
        fnAddr(basic_style_blink),
        fnAddr(basic_style_reverse),
        fnAddr(basic_style_reset),
        fnAddr(basic_screen_alternate),
        fnAddr(basic_screen_main),
        fnAddr(basic_get_cursor_pos),
        fnAddr(basic_flush),
        fnAddr(basic_begin_draw),
        fnAddr(basic_end_draw),
        fnAddr(terminal_get_width),
        fnAddr(terminal_get_height),
        fnAddr(basic_kbraw),
        fnAddr(basic_kbecho),
        fnAddr(basic_kbhit),
        fnAddr(basic_kbget),
        fnAddr(basic_kbpeek),
        fnAddr(basic_kbcode),
        fnAddr(basic_kbspecial),
        fnAddr(basic_kbmod),
        fnAddr(basic_kbflush),
        fnAddr(basic_kbclear),
        fnAddr(basic_kbcount),
        fnAddr(basic_inkey),
        fnAddr(basic_pos),
        fnAddr(basic_row),
        fnAddr(basic_csrlin),
        fnAddr(basic_mouse_enable),
        fnAddr(basic_mouse_disable),
        fnAddr(basic_mouse_x),
        fnAddr(basic_mouse_y),
        fnAddr(basic_mouse_buttons),
        fnAddr(basic_mouse_button),
        fnAddr(basic_mouse_poll),

        // ── Worker / Parallel ──
        fnAddr(worker_spawn),
        fnAddr(worker_spawn_messaging),
        fnAddr(worker_await),
        fnAddr(worker_ready),
        fnAddr(worker_args_alloc),
        fnAddr(worker_args_set_double),
        fnAddr(worker_args_set_int),
        fnAddr(worker_args_set_ptr),
        fnAddr(worker_future_outbox_offset),
        fnAddr(worker_future_inbox_offset),

        // ── Worker Messaging ──
        fnAddr(msg_queue_create),
        fnAddr(msg_queue_destroy),
        fnAddr(msg_queue_push),
        fnAddr(msg_queue_pop),
        fnAddr(msg_queue_has_message),
        fnAddr(msg_queue_close),
        fnAddr(msg_send_double),
        fnAddr(msg_send_int),
        fnAddr(msg_send_string),
        fnAddr(msg_send_udt),
        fnAddr(msg_send_marshalled),
        fnAddr(msg_send_udt_typed),
        fnAddr(msg_send_class),
        fnAddr(msg_receive_double),
        fnAddr(msg_receive_int),
        fnAddr(msg_receive_string),
        fnAddr(msg_receive_udt),
        fnAddr(msg_receive_marshalled),
        fnAddr(msg_cancel),
        fnAddr(msg_is_cancelled),
        fnAddr(msg_get_outbox),
        fnAddr(msg_get_inbox),
        fnAddr(msg_drain_and_destroy),
        fnAddr(msg_marshall_double),
        fnAddr(msg_marshall_int),
        fnAddr(msg_marshall_signal),
        fnAddr(msg_marshall_udt_typed),
        fnAddr(msg_marshall_class),
        fnAddr(msg_marshall_string),
        fnAddr(msg_marshall_array),
        fnAddr(msg_blob_free),
        fnAddr(msg_blob_tag),
        fnAddr(msg_blob_type_id),
        fnAddr(msg_blob_payload_ptr),
        fnAddr(msg_blob_forward),
        fnAddr(msg_unmarshall_double),
        fnAddr(msg_unmarshall_int),
        fnAddr(msg_unmarshall_string),
        fnAddr(msg_unmarshall_udt),
        fnAddr(msg_unmarshall_array),

        // ── Marshalling ──
        fnAddr(marshall_array),
        fnAddr(marshall_udt),
        fnAddr(marshall_udt_deep),
        fnAddr(unmarshall_array),
        fnAddr(unmarshall_udt),
        fnAddr(unmarshall_udt_deep),

        // ── Additional I/O ──
        fnAddr(basic_print_long),
        fnAddr(basic_print_float),
        fnAddr(basic_print_string),
        fnAddr(basic_print_cstr),
        fnAddr(basic_print_hex),
        fnAddr(basic_print_pointer),
        fnAddr(basic_print_at),
        fnAddr(basic_input_prompt),
        fnAddr(basic_input_line),
        fnAddr(basic_line_input),
        fnAddr(basic_eof),
        fnAddr(file_open),
        fnAddr(file_close),
        fnAddr(file_print_string),
        fnAddr(file_print_int),
        fnAddr(file_print_newline),
        fnAddr(file_print_double),
        fnAddr(file_read_line),
        fnAddr(file_eof),
        fnAddr(file_get_handle),
        fnAddr(file_set_handle),
        fnAddr(basic_system),
        fnAddr(basic_shell),
        fnAddr(basic_slurp),
        fnAddr(basic_spit),

        // ── String ops (legacy BasicString) ──
        fnAddr(str_new),
        fnAddr(str_retain),
        fnAddr(str_release),
        fnAddr(str_concat),
        fnAddr(str_compare),
        fnAddr(str_length),

        // ── Conversion ──
        fnAddr(int_to_str),
        fnAddr(long_to_str),
        fnAddr(float_to_str),
        fnAddr(double_to_str),
        fnAddr(str_to_int),
        fnAddr(str_to_long),
        fnAddr(str_to_float),
        fnAddr(str_to_double),

        // ── Extended string ops ──
        fnAddr(string_left),
        fnAddr(string_right),
        fnAddr(string_replace),
        fnAddr(string_reverse),
        fnAddr(string_new_ascii),
        fnAddr(string_new_capacity),
        fnAddr(string_space),
        fnAddr(string_repeat),

        // ── Additional math ──
        fnAddr(basic_abs_double),
        fnAddr(basic_sqrt),
        fnAddr(basic_pow),
        fnAddr(basic_sin),
        fnAddr(basic_cos),
        fnAddr(basic_tan),
        fnAddr(basic_asin),
        fnAddr(basic_acos),
        fnAddr(basic_atan),
        fnAddr(basic_atan2),
        fnAddr(basic_log),
        fnAddr(basic_log10),
        fnAddr(basic_exp),
        fnAddr(basic_floor),
        fnAddr(basic_ceil),
        fnAddr(basic_int),
        fnAddr(basic_fix),
        fnAddr(basic_rand),
        fnAddr(basic_rnd_int),
        fnAddr(basic_randomize),
        fnAddr(basic_round),

        // ── Bare C math aliases (QBE IL emits $sqrt, $pow, etc.) ──
        fnAddr(basic_sqrt), // _sqrt   → basic_sqrt
        fnAddr(basic_pow), // _pow    → basic_pow
        fnAddr(basic_abs_double), // _fabs   → basic_abs_double
        fnAddr(basic_sin), // _sin    → basic_sin
        fnAddr(basic_cos), // _cos    → basic_cos
        fnAddr(basic_tan), // _tan    → basic_tan
        fnAddr(basic_asin), // _asin   → basic_asin
        fnAddr(basic_acos), // _acos   → basic_acos
        fnAddr(basic_atan), // _atan   → basic_atan
        fnAddr(basic_atan2), // _atan2  → basic_atan2
        fnAddr(basic_log), // _log    → basic_log
        fnAddr(basic_log10), // _log10  → basic_log10
        fnAddr(basic_exp), // _exp    → basic_exp
        fnAddr(basic_floor), // _floor  → basic_floor
        fnAddr(basic_ceil), // _ceil   → basic_ceil
        fnAddr(basic_round), // _round  → basic_round
        fnAddr(basic_fix), // _trunc  → basic_fix (trunc)
        fnAddr(libm_sinh), // _sinh
        fnAddr(libm_cosh), // _cosh
        fnAddr(libm_tanh), // _tanh
        fnAddr(libm_hypot), // _hypot
        fnAddr(libm_cbrt), // _cbrt
        fnAddr(math_cint), // _math_cint

        // ── Error / exception ──
        fnAddr(basic_error_msg),
        fnAddr(basic_throw),
        fnAddr(basic_exception_push),
        fnAddr(basic_exception_pop),
        fnAddr(basic_err),
        fnAddr(basic_erl),
        fnAddr(basic_setjmp),

        // ── Globals ──
        fnAddr(basic_global_init),
        fnAddr(basic_global_base),
        fnAddr(basic_global_cleanup),

        // ── Array operations extended ──
        fnAddr(fbc_array_redim),
        fnAddr(fbc_array_redim_preserve),
        fnAddr(fbc_array_erase),
        fnAddr(fbc_array_lbound),
        fnAddr(fbc_array_ubound),

        // ── SAMM extended ──
        fnAddr(samm_track),
        fnAddr(samm_track_object),
        fnAddr(samm_untrack),
        fnAddr(samm_alloc_object),
        fnAddr(samm_free_object),
        fnAddr(samm_alloc_string),
        fnAddr(samm_track_string),
        fnAddr(samm_alloc_list),
        fnAddr(samm_track_list),
        fnAddr(samm_alloc_list_atom),
        fnAddr(samm_is_enabled),
        fnAddr(samm_scope_depth),
        fnAddr(samm_print_stats),

        // ── Timer SEND ──
        fnAddr(timer_after_send),
        fnAddr(timer_every_send),
        fnAddr(timer_stop),
        fnAddr(timer_stop_all),

        // ── Binary I/O ──
        fnAddr(file_put_record),
        fnAddr(file_get_record),
        fnAddr(file_seek),
        fnAddr(basic_loc),
        fnAddr(basic_lof),

        // ── MK$/CV$ binary conversion ──
        fnAddr(basic_mki),
        fnAddr(basic_mks),
        fnAddr(basic_mkd),
        fnAddr(basic_cvi),
        fnAddr(basic_cvs),
        fnAddr(basic_cvd),

        // ── Command-line arguments ──
        fnAddr(basic_command_count),
        fnAddr(basic_command),
    };

    for (0..entry_names.len) |i| {
        entries_storage[i] = JumpTableEntry{
            .name = entry_names[i],
            .address = addrs[i],
        };
    }
    entries_initialized = true;
}

/// Get the jump table entries slice, initializing on first call.
pub fn getEntries() []const JumpTableEntry {
    if (!entries_initialized) {
        initEntries();
    }
    return &entries_storage;
}

/// Build a RuntimeContext populated with real runtime function addresses.
///
/// Usage from the --jit CLI path:
///
///   const stubs = @import("jit_stubs.zig");
///   const ctx = stubs.buildJitRuntimeContext();
///   var session = try JitSession.compileFromModule(allocator, module, &ctx, ...);
///
pub fn buildJitRuntimeContext() RuntimeContext {
    return RuntimeContext{
        .entries = getEntries(),
        .dlsym_handle = null, // dlsym fallback for anything not in the table
    };
}

// ============================================================================
// Section: Tests
// ============================================================================

test "jump table has entries" {
    const entries = getEntries();
    try std.testing.expect(entries.len > 100);
    try std.testing.expectEqual(NUM_ENTRIES, entries.len);
}

test "buildJitRuntimeContext returns populated context" {
    const ctx = buildJitRuntimeContext();
    try std.testing.expect(ctx.entries.len > 0);
    try std.testing.expectEqual(NUM_ENTRIES, ctx.entries.len);
}

test "runtime context can look up core symbols" {
    const ctx = buildJitRuntimeContext();

    // Lifecycle
    try std.testing.expect(ctx.lookup("_basic_runtime_init") != null);
    try std.testing.expect(ctx.lookup("_samm_init") != null);
    try std.testing.expect(ctx.lookup("_samm_shutdown") != null);

    // I/O
    try std.testing.expect(ctx.lookup("_basic_print_int") != null);
    try std.testing.expect(ctx.lookup("_basic_print_newline") != null);
    try std.testing.expect(ctx.lookup("_basic_print_string_desc") != null);

    // Strings
    try std.testing.expect(ctx.lookup("_string_new_utf8") != null);
    try std.testing.expect(ctx.lookup("_string_concat") != null);
    try std.testing.expect(ctx.lookup("_string_release") != null);

    // Arrays
    try std.testing.expect(ctx.lookup("_fbc_array_create") != null);
}

test "runtime context lookup returns null for unknown symbol" {
    const ctx = buildJitRuntimeContext();
    try std.testing.expect(ctx.lookup("_nonexistent_function_xyz") == null);
}

test "all entries have non-empty names" {
    const entries = getEntries();
    for (entries) |e| {
        try std.testing.expect(e.name.len > 0);
    }
}

test "all entries have non-zero addresses" {
    const entries = getEntries();
    for (entries) |e| {
        try std.testing.expect(e.address != 0);
    }
}

test "all entry names start with underscore" {
    const entries = getEntries();
    for (entries) |e| {
        try std.testing.expect(e.name[0] == '_');
    }
}
