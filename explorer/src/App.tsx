import { Routes, Route } from 'react-router-dom';
import Layout from './components/Layout';
import Home from './pages/Home';
import Blocks from './pages/Blocks';
import BlockDetail from './pages/BlockDetail';
import Transaction from './pages/Transaction';
import Account from './pages/Account';

export default function App() {
  return (
    <Routes>
      <Route element={<Layout />}>
        <Route path="/" element={<Home />} />
        <Route path="/blocks" element={<Blocks />} />
        <Route path="/block/:height" element={<BlockDetail />} />
        <Route path="/tx/:hash" element={<Transaction />} />
        <Route path="/account/:address" element={<Account />} />
      </Route>
    </Routes>
  );
}
