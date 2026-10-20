//! Help text, preserved verbatim from `src/cli.js:13-62`.

use pihub_contracts::APP_VERSION;

pub fn help() -> String {
    format!(
        "pi-subagent-hub {APP_VERSION}\n\
\n\
Usage:\n\
\x20 pihub daemon start|stop|status [--json]\n\
\x20 pihub spawn --name NAME --cwd DIR [--model PROVIDER/MODEL] [--thinking LEVEL] [--prompt TEXT] [--json]\n\
\x20 pihub spawn --manifest FILE|- [--json]\n\
\x20 pihub list|ps [--json]\n\
\x20 pihub inspect AGENT_ID [--json]\n\
\x20 pihub prompt|send AGENT_ID [TEXT|--stdin] [--json]\n\
\x20 pihub steer AGENT_ID [TEXT|--stdin] [--json]\n\
\x20 pihub follow-up AGENT_ID [TEXT|--stdin] [--json]\n\
\x20 pihub abort|stop|restart AGENT_ID [--json]\n\
\x20 pihub wait AGENT_ID... [--timeout SECONDS] [--json]\n\
\x20 pihub logs AGENT_ID [--after CURSOR] [--json]\n\
\x20 pihub panel [--no-open]\n\
\x20 pihub doctor [--json]\n\
\x20 pihub config init|validate|paths [--json]\n\
\x20 pihub carriers list [--json]\n\
\x20 pihub providers list [--json]\n\
\x20 pihub providers add --stdin [--json]  # JSON with app, URL, models and API key\n\
\x20 pihub providers switch --app pi|codex|claude --id ID [--json]\n\
\x20 pihub skills list|import --id ID --source DIR [--label NAME] [--json]\n\
\x20 pihub skills manager status|connect|sync|disconnect [--cli-path FILE] [--preset ID,ID] [--ids ID,ID] [--tags TAG,TAG] [--json]\n\
\x20 pihub prompts list [--json]\n\
\x20 pihub agents create --carrier pi|codex|claude --name NAME --cwd DIR [--skills all|none|ID,ID] [--principles all|none|ID,ID] [--system-prompt TEXT|--system-prompt-file FILE|--system-prompt-stdin] [--json]\n\
\x20 pihub agents create --manifest FILE|- [--json]\n\
\x20 pihub agents list|inspect AGENT_ID [--json]\n\
\x20 pihub agents prompt AGENT_ID [TEXT|--stdin] [--json]\n\
\x20 pihub agents wait AGENT_ID... [--timeout SECONDS] [--json]\n\
\x20 pihub agents logs AGENT_ID [--after CURSOR] [--json]\n\
\x20 pihub agents abort|stop|restart AGENT_ID [--json]\n\
\n\
Machine interface:\n\
\x20 --json keeps stdout as one stable JSON envelope. Diagnostics go to stderr.\n\
\x20 Long prompts should use --stdin to avoid shell quoting and command-history exposure.\n\
\x20 System prompt text is never executed: it is passed as one argv element or read from\n\
\x20 a file/stdin. Long multi-line text should use --system-prompt-file or\n\
\x20 --system-prompt-stdin for the same reason.\n"
    )
}
