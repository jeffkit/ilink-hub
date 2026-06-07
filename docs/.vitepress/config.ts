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
    logo: '/logo.svg',
    siteTitle: 'iLink Hub',

    nav: [
      { text: '快速入门', link: '/guide/getting-started' },
      { text: '配置参考', link: '/reference/configuration' },
      { text: 'API 参考', link: '/reference/api' },
      {
        text: 'v0.1.4',
        items: [
          { text: '更新日志', link: 'https://github.com/jeffkit/ilink-hub/releases' },
          { text: '贡献指南', link: 'https://github.com/jeffkit/ilink-hub/blob/main/CONTRIBUTING.md' },
        ],
      },
    ],

    sidebar: [
      {
        text: '入门指南',
        items: [
          { text: '什么是 iLink Hub？', link: '/guide/what-is-ilink-hub' },
          { text: '快速开始', link: '/guide/getting-started' },
          { text: '安装', link: '/guide/installation' },
          { text: 'QR 码登录（可选）', link: '/guide/login' },
          { text: '注册客户端', link: '/guide/register-client' },
          { text: '手机扫码配对', link: '/guide/pairing-tunnel' },
          { text: '配置 AI 客户端', link: '/guide/client-config' },
        ],
      },
      {
        text: '部署',
        items: [
          { text: 'Docker 部署', link: '/deployment/docker' },
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
          { text: '与同类项目对比', link: '/guide/comparison' },
          { text: 'SDK 兼容性与推进动态', link: '/guide/sdk-compatibility' },
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
