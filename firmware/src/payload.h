#pragma once
#include "data.h"

// Daemon → device JSON payload (one line written to the RX characteristic).
// Pure: no Arduino, no Serial, so it runs in the native unit tests.
//
// Every key is optional. Absent keys parse to the same defaults the firmware
// has always used, so an old daemon produces exactly the UsageData it always
// did, with the fast-clock fields reading "absent" (activity -1, event '\0',
// has_event_seq false, sessions -1, replay false).
//
//   s  sr  w  wr  st  c  b  m  x  acct  tp  pd  rd  t  tf  ok   (long-standing)
//   a   activity level 0-3 (clamped)          e   latest event, first char used
//   es  event sequence number (uint32)        n   active sessions (clamped 0..999)
//   rp  1 = usage numbers are a replay        lv  liveliness 1-5 (clamped), -1 absent
//   aa  attention animation seconds 0-120 (clamped, 0 = off), -1 absent
//   pm  pace marker on the usage bars, 1 on / 0 off, -1 absent
//   pv  whose numbers: "cl" Claude, "cx" Codex, "cu" Cursor (up to 3 chars
//       kept), "" absent
//
// Returns false on malformed JSON; `*err` (if non-null) then points at a
// static description. `out` is fully overwritten on success, untouched on
// failure.
bool payload_parse(const char* json, UsageData* out, const char** err);

// Whose numbers `pv` names. AGENT_NONE when absent: an app with only Claude
// Code set up, which says nothing (and gets no agent switch on the device).
// An unknown code counts as absent.
enum Agent { AGENT_NONE, AGENT_CLAUDE, AGENT_CODEX, AGENT_CURSOR };
Agent payload_agent(const char* pv);

// The meters' title without a clock for `pv`: "Claude", "Codex", "Cursor",
// or "Usage" when absent or unknown (TITLE_TEXT in ui_text.h).
const char* payload_provider_title(const char* pv);

// Two `pv` values name the same agent; absent counts as Claude, which is
// what an app without Codex set up means.
bool payload_same_provider(const char* a, const char* b);
