/* Open Live Ingest for iPhone: the Rust core's C ABI (docs/specs/ios-app.md, 8.1).
 *
 * Written by hand; mobile/src/lib.rs is the implementation. Every message is one
 * UTF-8 JSON string in the desktop app's envelope:
 *   Swift -> core:  {"id": <u32>, "cmd": "<name>", "args": {...}}
 *   core -> Swift:  {"id": <u32>, "ok": <value>}
 *                   {"id": <u32 or null>, "error": {"message": "...", "action": null}}
 *                   {"event": "<name>", "data": {...}}
 * A panic inside the core is caught and becomes {"event": "fatal", ...}. */
#ifndef OLI_H
#define OLI_H

#ifdef __cplusplus
extern "C" {
#endif

typedef struct OliCore OliCore;

/* Called on a core thread, any thread, with one UTF-8 JSON message (a reply or an
   event). The string is valid only during the call; Swift copies it. */
typedef void (*oli_deliver)(void *ctx, const char *json);

/* Starts the core: settings in <app_support_dir>/open-live-ingest/, the log in
   <log_dir>/app.log. NULL on failure. deliver may be called, with ctx, until
   oli_core_free returns. */
OliCore *oli_core_new(const char *app_support_dir, const char *log_dir,
                      oli_deliver deliver, void *ctx);

/* One command. Returns at once; the reply follows through deliver. */
void oli_core_send(OliCore *core, const char *json);

/* Stops what is live, waits at most 4 s for its teardown, then frees the core.
   Not to be called from within deliver. */
void oli_core_free(OliCore *core);

/* Reserved: nothing returns an owned string today. */
void oli_string_free(char *s);

#ifdef __cplusplus
}
#endif

#endif /* OLI_H */
