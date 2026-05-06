#!/usr/bin/env bash
set -euo pipefail

PROJECT_INPUT="${1:-$(pwd -P)}"
if [[ ! -d "$PROJECT_INPUT" ]]; then
  echo "Project directory does not exist: ${PROJECT_INPUT}" >&2
  exit 2
fi
PROJECT="$(cd "$PROJECT_INPUT" && pwd -P)"
AGENT_MODEL="${AGENT_MODEL:-gemini-2.5-pro}"
AGENT_TASK="${AGENT_TASK:-Software Engineering Agent}"
AGENT_NAME="${AGENT_NAME:-}"

echo "Registering Gemini agent in ${PROJECT}..."

am setup run --agent gemini --project-dir "$PROJECT" --yes

register_args=(
  agents register
  --project "$PROJECT"
  --program "gemini-cli"
  --model "$AGENT_MODEL"
  --task "$AGENT_TASK"
  --attachments-policy "auto"
)

if [[ -n "$AGENT_NAME" ]]; then
  register_args+=(--name "$AGENT_NAME")
fi

am "${register_args[@]}"

if [[ -n "$AGENT_NAME" ]]; then
  echo "Registration complete. Agent: ${AGENT_NAME}"
  echo "To introduce yourself, use: am mail send --project '$PROJECT' --from '$AGENT_NAME' --to <recipient> --subject 'Hello' --body '...'"
else
  echo "Registration complete. Use the generated agent name printed above when sending mail."
  echo "To introduce yourself, use: am mail send --project '$PROJECT' --from <generated-agent-name> --to <recipient> --subject 'Hello' --body '...'"
fi
