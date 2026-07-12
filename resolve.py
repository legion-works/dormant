#!/usr/bin/env python3
"""Resolve merge conflicts where HEAD has 'watchdog:' and origin/dev has 'audio:' by keeping both."""
import re
import sys
import os

files = [
    "crates/dormant-core/src/config/schema.rs",
    "crates/dormant-core/src/config/validate.rs",
    "crates/dormant-core/src/zone.rs",
    "crates/dormant-doctor/src/service.rs",
    "crates/dormant-web/src/lib.rs",
    "crates/dormant-web/src/routes/command.rs",
    "crates/dormant-web/src/routes/config.rs",
    "crates/dormant-web/src/routes/config_apply.rs",
    "crates/dormant-web/src/routes/doctor.rs",
    "crates/dormant-web/src/routes/events.rs",
    "crates/dormant-web/src/routes/wear.rs",
    "crates/dormant-web/src/security.rs",
    "crates/dormant-web/src/server.rs",
    "crates/dormantd/src/app.rs",
    "crates/dormantd/src/ipc.rs",
    "crates/dormantd/src/reload.rs",
    "crates/dormantd/src/wear_tracker.rs",
    "crates/dormantd/tests/ipc_roundtrip.rs",
    "crates/dormant-web/webui/src/api/types.ts",
    "crates/dormant-web/webui/src/app/config/SettingsForm.tsx",
    "ARCHITECTURE.md",
    "ROADMAP.md",
]

for fpath in files:
    with open(fpath, 'r') as f:
        content = f.read()
    original = content
    
    # Pattern 1: Config struct literal — watchdog vs audio field
    # This handles the case where HEAD has watchdog: ... and dev has audio: ...
    # Resolution: keep both lines
    pattern = re.compile(
        r'<<<<<<< HEAD\n'
        r'((?:[ \t]*)(?:watchdog|/// Watchdog|/// Crash-loop)[^\n]*\n'
        r'(?:(?:[ \t]*)(?:watchdog|/// |pub |#[^\n]*|#[^\n]*\n[ \t]*/// )[^\n]*\n)*)'
        r'=======\n'
        r'((?:[ \t]*)(?:audio|/// Audio|/// Global)[^\n]*\n'
        r'(?:(?:[ \t]*)(?:audio|/// |pub |#[^\n]*|#[^\n]*\n[ \t]*/// )[^\n]*\n)*)'
        r'>>>>>>> origin/dev\n',
        re.MULTILINE
    )
    
    # Simpler approach: find each conflict block and handle it
    lines = content.split('\n')
    result_lines = []
    i = 0
    while i < len(lines):
        line = lines[i]
        if line.startswith('<<<<<<< HEAD'):
            # Collect HEAD side
            head_lines = []
            i += 1
            while i < len(lines) and not lines[i].startswith('======='):
                head_lines.append(lines[i])
                i += 1
            i += 1  # skip =======
            # Collect origin/dev side
            dev_lines = []
            while i < len(lines) and not lines[i].startswith('>>>>>>> origin/dev'):
                dev_lines.append(lines[i])
                i += 1
            i += 1  # skip >>>>>>>
            
            # Resolve: keep both sides' content (non-empty lines)
            # Strip trailing whitespace but keep indentation
            for hl in head_lines:
                if hl.strip():
                    result_lines.append(hl)
            for dl in dev_lines:
                if dl.strip():
                    result_lines.append(dl)
        else:
            result_lines.append(line)
            i += 1
    
    content = '\n'.join(result_lines)
    
    if content != original:
        with open(fpath, 'w') as f:
            f.write(content)
        print(f"Resolved: {fpath}")
    else:
        print(f"Unchanged: {fpath}")
