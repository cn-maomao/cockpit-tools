#!/usr/bin/env python
"""Cockpit Tools machine-readable adapter for grok-register."""

import argparse
import json
import os
import signal
import sys
import traceback


def configure_standard_streams():
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if callable(reconfigure):
            try:
                reconfigure(encoding="utf-8", errors="backslashreplace", line_buffering=True)
            except (OSError, ValueError):
                pass


def emit(event_type, **payload):
    value = {"type": event_type, **payload}
    line = json.dumps(value, ensure_ascii=False)
    try:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()
    except (BrokenPipeError, OSError, ValueError):
        # A PyInstaller console process launched without a visible Windows
        # console can occasionally lose its stdout text wrapper. stderr is a
        # separately piped machine-event fallback understood by Cockpit.
        try:
            sys.stderr.write(line + "\n")
            sys.stderr.flush()
        except (BrokenPipeError, OSError, ValueError):
            pass


def main():
    configure_standard_streams()
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--cancel-file", required=True)
    args = parser.parse_args()

    import app_config

    app_config.CONFIG_FILE = os.path.abspath(args.config)
    import grok_register_ttk as core

    stopped = {"value": False}

    def request_stop(_signum=None, _frame=None):
        stopped["value"] = True
        emit("state", state="stopping")

    signal.signal(signal.SIGINT, request_stop)
    signal.signal(signal.SIGTERM, request_stop)

    def log(message):
        text = str(message)
        if "邮箱credential" in text:
            text = text.split(":", 1)[0] + ": [redacted]"
        emit("log", message=text)

    try:
        app_config.load_config()
        validated = app_config.validate_run_requirements(app_config.config)
        app_config.config.clear()
        app_config.config.update(validated)
        # Cockpit performs the current grok2api import itself so that credentials
        # never need to be placed in the registration sidecar configuration.
        app_config.config["grok2api_auto_add_local"] = False
        app_config.config["grok2api_auto_add_remote"] = False
        count = int(app_config.config.get("register_count", 1) or 1)
        emit("state", state="running", count=count)

        def observer(batch, account, output):
            emit(
                "progress",
                success=batch.success_count,
                failed=batch.fail_count,
                pending=batch.registered_unsaved_count,
                warnings=batch.postprocess_warning_count,
                processed=batch.processed_count,
                total=count,
            )
            if account and account.ok and output and output.registered:
                emit(
                    "account",
                    email=account.email,
                    password=account.password,
                    sso=account.sso,
                )

        batch = core.run_registration_common(
            count=count,
            log_callback=log,
            cancel_callback=lambda: stopped["value"] or os.path.exists(args.cancel_file),
            accounts_output_file=os.path.abspath(args.output),
            observer=observer,
        )
        emit(
            "complete",
            success=batch.success_count,
            failed=batch.fail_count,
            pending=batch.registered_unsaved_count,
            warnings=batch.postprocess_warning_count,
            cancelled=batch.cancelled or stopped["value"],
        )
        return 0 if batch.success_count > 0 or batch.cancelled else 2
    except Exception as exc:
        emit("error", message=str(exc), detail=traceback.format_exc(limit=8))
        return 1
    finally:
        try:
            core.cleanup_runtime_memory(log_callback=log, reason="cockpit-sidecar-exit")
        except Exception as exc:
            emit("log", message=f"清理浏览器运行时失败: {exc}")


if __name__ == "__main__":
    sys.exit(main())
