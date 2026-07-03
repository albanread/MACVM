/* qbe_bridge.c — Library API for embedding QBE in the FasterBASIC Zig compiler
 *
 * This file replaces QBE's main.c with a callable library interface.
 * It defines the global state that QBE's internal modules expect (Target T,
 * char debug[], etc.) and provides qbe_compile_il() which runs the full
 * QBE optimization + emission pipeline on a QBE IL text buffer.
 *
 * The optimization pipeline is identical to upstream QBE's main.c func().
 */

#include "all.h"
#include "config.h"
#include "qbe_bridge.h"
#include <string.h>

/* ── Global state required by QBE internals ─────────────────────────────
 *
 * These are declared `extern` in all.h but defined in main.c.
 * Since we don't compile main.c, we define them here.
 */
Target T;

char debug['Z'+1] = {
	['P'] = 0, /* parsing */
	['M'] = 0, /* memory optimization */
	['N'] = 0, /* ssa construction */
	['C'] = 0, /* copy elimination */
	['F'] = 0, /* constant folding */
	['K'] = 0, /* if-conversion */
	['A'] = 0, /* abi lowering */
	['I'] = 0, /* instruction selection */
	['L'] = 0, /* liveness */
	['S'] = 0, /* spilling */
	['R'] = 0, /* reg. allocation */
};

/* ── Target table ───────────────────────────────────────────────────────*/

extern Target T_amd64_sysv;
extern Target T_amd64_apple;
extern Target T_arm64;
extern Target T_arm64_apple;
extern Target T_rv64;

static Target *tlist[] = {
	&T_amd64_sysv,
	&T_amd64_apple,
	&T_arm64,
	&T_arm64_apple,
	&T_rv64,
	0
};

/* ── Per-compilation state (file-scoped) ────────────────────────────────*/

static FILE *bridge_outf;
static int bridge_dbg;

/* ── Callbacks for QBE's parse() ────────────────────────────────────────
 *
 * These are functionally identical to the ones in QBE's main.c.
 * parse() calls them for each data definition and function it encounters.
 */

static void
bridge_data_cb(Dat *d)
{
	if (bridge_dbg)
		return;
	emitdat(d, bridge_outf);
	if (d->type == DEnd) {
		fputs("/* end data */\n\n", bridge_outf);
		freeall();
	}
}

static void
bridge_func_cb(Fn *fn)
{
	uint n;

	if (bridge_dbg)
		fprintf(stderr, "**** Function %s ****", fn->name);
	if (debug['P']) {
		fprintf(stderr, "\n> After parsing:\n");
		printfn(fn, stderr);
	}

	/* ── Full QBE optimization pipeline (matches upstream main.c) ── */
	T.abi0(fn);
	fillcfg(fn);
	filluse(fn);
	promote(fn);
	filluse(fn);
	ssa(fn);
	filluse(fn);
	ssacheck(fn);
	fillalias(fn);
	loadopt(fn);
	filluse(fn);
	fillalias(fn);
	coalesce(fn);
	filluse(fn);
	filldom(fn);
	ssacheck(fn);
	gvn(fn);
	fillcfg(fn);
	simplcfg(fn);
	filluse(fn);
	filldom(fn);
	gcm(fn);
	filluse(fn);
	ssacheck(fn);
	if (T.cansel) {
		ifconvert(fn);
		fillcfg(fn);
		filluse(fn);
		filldom(fn);
		ssacheck(fn);
	}
	T.abi1(fn);
	simpl(fn);
	fillcfg(fn);
	filluse(fn);
	T.isel(fn);
	fillcfg(fn);
	filllive(fn);
	fillloop(fn);
	fillcost(fn);
	spill(fn);
	rega(fn);
	fillcfg(fn);
	simpljmp(fn);
	fillcfg(fn);
	filllive(fn); /* re-run after regalloc so b->out has physical regs */
	assert(fn->rpo[0] == fn->start);
	for (n=0;; n++)
		if (n == fn->nblk-1) {
			fn->rpo[n]->link = 0;
			break;
		} else
			fn->rpo[n]->link = fn->rpo[n+1];
	if (!bridge_dbg) {
		T.emitfn(fn, bridge_outf);
		fprintf(bridge_outf, "/* end function %s */\n\n", fn->name);
	} else
		fprintf(stderr, "\n");
	freeall();
}

static void
bridge_dbgfile_cb(char *fn)
{
	emitdbgfile(fn, bridge_outf);
}

/* ── Internal: select target by name ────────────────────────────────────*/

static int
select_target(const char *name)
{
	Target **t;

	if (!name) {
		T = Deftgt;
		return 0;
	}
	for (t = tlist; *t; t++) {
		if (strcmp(name, (*t)->name) == 0) {
			T = **t;
			return 0;
		}
	}
	return -1;
}

/* ── Internal: run QBE pipeline on a FILE* ──────────────────────────────*/

static int
compile_from_file(FILE *inf, FILE *outf, const char *target_name)
{
	if (select_target(target_name) != 0)
		return QBE_ERR_TARGET;

	memset(debug, 0, sizeof(debug));
	bridge_dbg = 0;
	bridge_outf = outf;

	parse(inf, "<il>", bridge_dbgfile_cb, bridge_data_cb, bridge_func_cb);

	if (!bridge_dbg)
		T.emitfin(outf);

	return QBE_OK;
}

/* ═══════════════════════════════════════════════════════════════════════
 * Public API
 * ═══════════════════════════════════════════════════════════════════════*/

int
qbe_compile_il(const char *il_text, size_t il_len,
               const char *asm_path, const char *target_name)
{
	FILE *inf, *outf;
	int rc;

	if (!il_text || il_len == 0)
		return QBE_ERR_INPUT;

	outf = fopen(asm_path, "w");
	if (!outf)
		return QBE_ERR_OUTPUT;

	inf = fmemopen((void *)il_text, il_len, "r");
	if (!inf) {
		fclose(outf);
		return QBE_ERR_OUTPUT;
	}

	rc = compile_from_file(inf, outf, target_name);

	fclose(inf);
	fclose(outf);
	return rc;
}

int
qbe_compile_il_to_file(const char *il_text, size_t il_len,
                       void *output_file, const char *target_name)
{
	FILE *inf;
	int rc;

	if (!il_text || il_len == 0)
		return QBE_ERR_INPUT;
	if (!output_file)
		return QBE_ERR_OUTPUT;

	inf = fmemopen((void *)il_text, il_len, "r");
	if (!inf)
		return QBE_ERR_INPUT;

	rc = compile_from_file(inf, (FILE *)output_file, target_name);

	fclose(inf);
	return rc;
}

const char *
qbe_default_target(void)
{
	/* Must use static storage so the embedded name[] array survives */
	static Target def;
	static int init = 0;
	if (!init) {
		def = Deftgt;
		init = 1;
	}
	return def.name;
}

const char **
qbe_available_targets(void)
{
	static const char *names[8];  /* max 5 targets + NULL */
	static int init = 0;

	if (!init) {
		int i;
		Target **t;
		for (t = tlist, i = 0; *t && i < 7; t++, i++)
			names[i] = (*t)->name;
		names[i] = 0;
		init = 1;
	}
	return names;
}

const char *
qbe_version(void)
{
	return VERSION;
}