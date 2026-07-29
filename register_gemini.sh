#!/bin/bash
set -e
PROJECT=$(pwd)
echo "Registering Gemini agent in $PROJECT..."

# Ensure project exists and config is set up
am setup run --project-dir "$PROJECT" --yes

# Register agent with a valid AdjectiveNoun name
# "Gemini" is a reserved program name, so we use "SwiftFox"
AGENT_NAME="SwiftFox"

am agents register 
  --project "$PROJECT" 
  --program "gemini-cli" 
  --model "gemini-2.0-flash" 
  --name "$AGENT_NAME" 
  --task "Software Engineering Agent" 
  --attachments-policy "auto"

echo "Registration complete. Agent: $AGENT_NAME"
echo "To introduce yourself, use: am mail send --project '$PROJECT' --from '$AGENT_NAME' --to <recipient> --subject 'Hello' --body '...'"
