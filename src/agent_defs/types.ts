export type {
  AgentDefinition,
  AgentDefinitionEntry,
  AgentMcpServer,
  AgentMcpTransportHttp,
  AgentMcpTransportStdio,
  ToolPolicy,
} from "@tyde/protocol";

declare module "@tyde/protocol" {
  interface AgentDefinition {
    /** Skill names from ~/.tyde/skills/, resolved and injected at launch. */
    skill_names?: string[];
  }
}
