#include "signals.h"
#include "payload.h"
#include "usage_rate.h"
#include <string.h>

// Pure: no Arduino. See signals.h.

static bool     hooks_seen = false;
static bool     have_seq   = false;
static uint32_t last_seq   = 0;
static char     last_event = '\0';
static int      sessions   = -1;
static char     sample_pv[sizeof(UsageData::provider)] = "";   // `pv` of the last real sample

void signals_clear(void) {
    hooks_seen = false;
    have_seq = false;
    last_seq = 0;
    last_event = '\0';
    sessions = -1;
    sample_pv[0] = '\0';
    usage_rate_clear();
}

SignalsUpdate signals_ingest_at(const UsageData* d, uint32_t now_ms) {
    SignalsUpdate u = {};
    u.replay = d->replay;
    u.group_before = usage_rate_group_at(now_ms);

    // Activity first so the group_after below already carries the lift.
    // Replays refresh it too: the daemon is alive and that is its level.
    if (d->activity >= 0) {
        hooks_seen = true;
        usage_rate_set_activity_at(now_ms, d->activity);
    }

    // A failed poll is not a zero: an {"ok":false} "no data" beat carries no
    // session % (it parses as 0), so it must not reach the rate ring or the
    // reset detector (it would read as a fresh window and fire the chime).
    const bool real_sample = d->ok && !d->replay;
    // The device switched agents (Claude ↔ Codex): their numbers are a new
    // series, so a lower session % there is not a reset.
    if (real_sample && !payload_same_provider(sample_pv, d->provider)) usage_rate_rebase();
    if (real_sample) memcpy(sample_pv, d->provider, sizeof(sample_pv));
    u.session_reset = usage_rate_sample_at(now_ms, d->session_pct, !real_sample);

    if (d->has_event_seq) {
        hooks_seen = true;
        if (!have_seq) {
            have_seq = true;                 // baseline, don't react
            last_seq = d->event_seq;
        } else if (d->event_seq != last_seq) {
            // Fires on a replay too: `rp` only says the usage numbers are
            // repeated. The app sends a fresh event in an `rp:1` write ahead
            // of the poll, and `es` alone keeps it from firing twice.
            last_seq = d->event_seq;
            u.event_fired = true;
            u.event = d->event;
            u.event_seq = d->event_seq;
            last_event = d->event;
        }
    }

    if (d->sessions >= 0) {
        hooks_seen = true;
        if (d->sessions != sessions) {
            sessions = d->sessions;
            u.sessions_changed = true;
        }
    }

    u.group_after = usage_rate_group_at(now_ms);
    return u;
}

bool     signals_hooks_seen(void) { return hooks_seen; }
int      signals_activity_at(uint32_t now_ms) { return usage_rate_activity_at(now_ms); }
int      signals_sessions(void) { return sessions; }
char     signals_last_event(void) { return last_event; }
