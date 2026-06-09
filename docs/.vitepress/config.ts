import { defineConfig } from 'vitepress'

export default defineConfig({
  title: 'iLink Hub',
  description: '将一个微信账号连接到多个 AI 后端的透明代理 — 零客户端改造',
  lang: 'zh-CN',

  // GitHub Pages 部署地址：https://jeffkit.github.io/ilink-hub/
  base: '/ilink-hub/',

  head: [
    ['link', { rel: 'icon', href: '/ilink-hub/favicon.svg' }],
    ['meta', { name: 'theme-color', content: '#6366f1' }],
  ],

  themeConfig: {
    logo: '/logo.png',
    siteTitle: 'iLink Hub',

    nav: [
      { text: '下载桌面版', link: '/guide/installation#desktop' },
      { text: '快速开始', link: '/guide/getting-started' },
      { text: 'Bridge', link: '/bridge/' },
      { text: '常见问题', link: '/guide/faq' },
      {
        text: 'v0.1.20',
        items: [
          { text: '更新日志', link: 'https://github.com/jeffkit/ilink-hub/releases' },
          { text: '贡献指南', link: 'https://github.com/jeffkit/ilink-hub/blob/main/CONTRIBUTING.md' },
        ],
      },
    ],

    sidebar: [
      {
        text: '开始使用',
        items: [
          { text: '什么是 iLink Hub？', link: '/guide/what-is-ilink-hub' },
          { text: '下载桌面应用', link: '/guide/installation#desktop' },
          { text: '快速开始（命令行）', link: '/guide/getting-started' },
          { text: '所有安装方式', link: '/guide/installation' },
          { text: '让 AI 帮你安装 ✨', link: '/guide/ai-install' },
        ],
      },
      {
        text: 'ilink-hub-bridge',
        collapsed: false,
        items: [
          { text: '什么是 bridge？', link: '/bridge/' },
          { text: '5 分钟上手（echo 验证）', link: '/bridge/quick-try' },
          { text: '接入 Claude Code ✨', link: '/guide/claude-code' },
          { text: '使用指引（多 CLI / 多项目）', link: '/bridge/USAGE' },
          { text: '功能与配置参考', link: '/bridge/README' },
          { text: '连接远程 Hub（SSH / launchd）', link: '/bridge/remote-hub' },
          {
            text: '开发自定义 Profile',
            collapsed: true,
            items: [
              { text: 'Profile 协议规范', link: '/bridge/profile-spec' },
              { text: 'Node.js 开发教程', link: '/bridge/develop-nodejs' },
              { text: 'Python 开发教程', link: '/bridge/develop-python' },
              { text: '完整示例（Claude/Cursor/Codex）', link: '/bridge/sdk-examples' },
            ],
          },
        ],
      },
      {
        text: '配置指南',
        items: [
          { text: '注册客户端', link: '/guide/register-client' },
          { text: '配置 AI 客户端', link: '/guide/client-config' },
          { text: '手机扫码配对', link: '/guide/pairing-tunnel' },
          { text: 'QR 码登录', link: '/guide/login' },
        ],
      },
      {
        text: '部署',
        items: [
          { text: 'Docker 部署', link: '/deployment/docker' },
          { text: 'Linux / VPS 部署（systemd）', link: '/deployment/linux-systemd' },
          { text: '安全建议', link: '/deployment/security' },
        ],
      },
      {
        text: '参考',
        items: [
          { text: '微信命令', link: '/reference/commands' },
          { text: '环境变量配置', link: '/reference/configuration' },
          { text: 'HTTP API', link: '/reference/api' },
          { text: 'Prometheus 指标', link: '/reference/metrics' },
        ],
      },
      {
        text: '其他',
        items: [
          { text: '常见问题 FAQ', link: '/guide/faq' },
          { text: 'SDK 兼容性说明', link: '/guide/sdk-compatibility' },
          { text: '与同类项目对比', link: '/guide/comparison' },
        ],
      },
    ],

    socialLinks: [
      { icon: 'github', link: 'https://github.com/jeffkit/ilink-hub' },
    ],

    footer: {
      message: 'Released under the MIT License.',
      copyright: 'Copyright © 2026 jeffkit',
    },

    search: {
      provider: 'local',
    },

    editLink: {
      pattern: 'https://github.com/jeffkit/ilink-hub/edit/main/docs/:path',
      text: '在 GitHub 上编辑此页',
    },

    lastUpdated: {
      text: '最后更新于',
    },
  },
})
