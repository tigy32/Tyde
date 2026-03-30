import { defineConfig } from 'vitepress'

export default defineConfig({
  title: 'Tyde',
  description: 'Documentation for Tyde — coding agent studio',

  themeConfig: {
    nav: [
      { text: 'Guide', link: '/getting-started' },
    ],

    sidebar: [
      {
        text: 'Getting Started',
        items: [
          { text: 'Introduction', link: '/getting-started' },
          { text: 'Workspace', link: '/workspace' },
        ],
      },
      {
        text: 'Backends',
        items: [
          { text: 'Tycode', link: '/backends/tycode' },
          { text: 'Claude Code', link: '/backends/claude-code' },
          { text: 'Codex', link: '/backends/codex' },
          { text: 'Kiro', link: '/backends/kiro' },
          { text: 'Gemini CLI', link: '/backends/gemini' },
        ],
      },
      {
        text: 'Features',
        items: [
          { text: 'Agent Definitions', link: '/features/agents' },
          { text: 'Agent Control', link: '/features/agent-control' },
          { text: 'Workbenches', link: '/features/workbenches' },
          { text: 'Workflows', link: '/features/workflows' },
        ],
      },
    ],

    socialLinks: [
      { icon: 'github', link: 'https://github.com/tigy32/Tyde' },
    ],
  },
})
