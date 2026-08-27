# Mermaid Test

\`\`\`
mermaid
flowchart TB
  subgraph bins["crates/atom — binaries"]
    A["atom<br/>TUI client"]
    S["atoms<br/>session server"]
  end

  subgraph libs["library crates"]
    TUI["atom-tui<br/>ratatui + crossterm"]
    SRV["atom-server<br/>background sessions"]
    CORE["atom-core<br/>shared types & helpers"]
    TOOLS["atom-tools<br/>tool execution"]
    SANDBOX["atom-sandbox<br/>sandboxing"]
  end

  A --> TUI
  A --> CORE
  A --> SRV
  S --> SRV
  S --> CORE
  TUI --> CORE
  SRV --> TOOLS
  SRV --> CORE
  TOOLS --> SANDBOX
  TOOLS --> CORE
  SANDBOX --> CORE

  A -. "spawns (managed, _ATOM_LAUNCH)" .-> S
  A -. "HTTP (hyper + reqwest)" .-> SRV

  style bins fill:#1e3a5f,stroke:#4a9eff,color:#fff
  style libs fill:#2d4a2d,stroke:#6fce6f,color:#fff
\`\`\`
