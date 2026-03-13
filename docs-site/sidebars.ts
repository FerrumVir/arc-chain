import type {SidebarsConfig} from '@docusaurus/plugin-content-docs';

const sidebars: SidebarsConfig = {
  docsSidebar: [
    {
      type: 'category',
      label: 'Getting Started',
      collapsed: false,
      items: ['quickstart', 'running-testnet'],
    },
    {
      type: 'category',
      label: 'Architecture',
      items: ['architecture'],
    },
    {
      type: 'category',
      label: 'API',
      items: ['rpc-api'],
    },
    {
      type: 'category',
      label: 'SDKs',
      items: ['sdk-typescript', 'sdk-python'],
    },
    {
      type: 'category',
      label: 'Advanced',
      items: ['smart-contracts', 'benchmarking'],
    },
    {
      type: 'doc',
      id: 'overview',
      label: 'Overview',
    },
  ],
};

export default sidebars;
