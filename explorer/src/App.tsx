import { Routes, Route } from 'react-router-dom';
import Layout from './components/Layout';
import Home from './pages/Home';
import Blockchain from './pages/Blockchain';
import Blocks from './pages/Blocks';
import BlockDetail from './pages/BlockDetail';
import Transaction from './pages/Transaction';
import Account from './pages/Account';
import Faucet from './pages/Faucet';
import Validators from './pages/Validators';

export default function App() {
  return (
    <Routes>
      <Route element={<Layout />}>
        <Route path="/" element={<Home />} />
        <Route path="/blockchain" element={<Blockchain />} />
        <Route path="/blocks" element={<Blocks />} />
        <Route path="/block/:height" element={<BlockDetail />} />
        <Route path="/tx/:hash" element={<Transaction />} />
        <Route path="/account/:address" element={<Account />} />
        <Route path="/faucet" element={<Faucet />} />
        <Route path="/validators" element={<Validators />} />
      </Route>
    </Routes>
  );
}
