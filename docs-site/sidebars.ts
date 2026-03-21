import type {SidebarsConfig} from '@docusaurus/plugin-content-docs';

const sidebars: SidebarsConfig = {
  docsSidebar: [
    'intro',
    {
      type: 'category',
      label: 'Getting Started',
      collapsed: false,
      items: ['quickstart', 'testnet'],
    },
    {
      type: 'category',
      label: 'Architecture',
      items: ['architecture'],
    },
    {
      type: 'category',
      label: 'AI Agents',
      items: ['agents/agents-overview', 'agents/deploy-agent'],
    },
    {
      type: 'category',
      label: 'API Reference',
      items: ['rpc-api'],
    },
    {
      type: 'category',
      label: 'SDKs',
      items: ['sdk/sdk-python'],
    },
    {
      type: 'category',
      label: 'Economics',
      items: ['tokenomics'],
    },
    {
      type: 'category',
      label: 'Performance',
      items: ['benchmarks'],
    },
  ],
};

export default sidebars;
