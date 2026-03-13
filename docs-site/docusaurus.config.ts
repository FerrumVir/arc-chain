import {themes as prismThemes} from 'prism-react-renderer';
import type {Config} from '@docusaurus/types';
import type * as Preset from '@docusaurus/preset-classic';

const config: Config = {
  title: 'ARC Chain Docs',
  tagline: 'ai for Humans First',
  favicon: 'img/favicon.ico',

  future: {
    v4: true,
  },

  url: 'https://docs.arc.tech',
  baseUrl: '/',

  organizationName: 'FerrumVir',
  projectName: 'arc-chain',

  onBrokenLinks: 'throw',

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  presets: [
    [
      'classic',
      {
        docs: {
          sidebarPath: './sidebars.ts',
          editUrl: 'https://github.com/FerrumVir/arc-chain/tree/main/docs-site/',
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      } satisfies Preset.Options,
    ],
  ],

  themeConfig: {
    colorMode: {
      defaultMode: 'dark',
      respectPrefersColorScheme: false,
    },
    navbar: {
      title: 'ARC Docs',
      items: [
        {
          type: 'docSidebar',
          sidebarId: 'docsSidebar',
          position: 'left',
          label: 'Documentation',
        },
        {
          href: 'https://explorer-nine-iota.vercel.app',
          label: 'Explorer',
          position: 'right',
        },
        {
          href: 'https://github.com/FerrumVir/arc-chain',
          label: 'GitHub',
          position: 'right',
        },
      ],
    },
    footer: {
      style: 'dark',
      copyright: '© 2026 ARC Chain',
    },
    prism: {
      theme: prismThemes.github,
      darkTheme: prismThemes.dracula,
      additionalLanguages: ['rust', 'toml', 'bash', 'python', 'json'],
    },
  } satisfies Preset.ThemeConfig,
};

export default config;
