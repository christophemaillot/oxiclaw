use anyhow::Result;
use std::fs;
use std::path::Path;

pub fn build_system_prompt(basedir: &Path, tools_catalog: &str) -> Result<String> {
    let soul = read_or_default(
        &basedir.join("SOUL.md"),
        "# SOUL

Tu es OxiClaw: clair, calme, utile.",
    );
    let identity = read_or_default(
        &basedir.join("IDENTITY.md"),
        "# IDENTITY

- Name: OxiClaw
- Engine: OxiClaw",
    );
    let user = read_or_default(
        &basedir.join("USER.md"),
        "# USER

- Name: Christophe
- Language: fr",
    );
    let agent = read_or_default(
        &basedir.join("AGENT.md"),
        "# AGENT

- SOUL.md est immuable
- Utilise info_append pour enrichir AGENT.md/USER.md",
    );

    let protocol = read_or_default(
        &basedir.join("conf").join("prompts").join("main_system.md"),
        default_main_protocol(),
    );

    Ok(format!(
        "{protocol}

{soul}

{identity}

{user}

{agent}

TOOLS DISPONIBLES (JSON):
{tools_catalog}",
    ))
}

fn default_main_protocol() -> &'static str {
    "PROTOCOLE DE SORTIE:
- Si tu as besoin d'un outil, réponds UNIQUEMENT en JSON strict:
  {\"type\":\"tool_call\",\"name\":\"time\",\"args\":{}}
- Si tu as fini, réponds UNIQUEMENT en JSON strict:
  {\"type\":\"final_answer\",\"answer\":\"...\"}
- Quand tu reçois TOOL_RESULT, tu peux soit appeler un autre tool, soit final_answer.
- N'invente jamais un tool absent du catalogue.
- Prise de décision: agir plutôt que bloquer; si une info manque, chercher en mémoire puis poser une seule question courte.
- SOUL.md est immuable: ne jamais tenter de le modifier.
- Si une information stable et utile est apprise, utiliser info_append(target,text) avec target=agent ou user."
}

fn read_or_default(path: &Path, default: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|_| default.to_string())
}
