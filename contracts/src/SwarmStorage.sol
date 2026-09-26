// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Sys, SystemContract} from "./System.sol";

interface IRewardsBurn {
    function burnSupply(uint256 amount) external;
}

interface IStakingHistory {
    function validator(uint32 id) external view returns (address, address, uint256, uint8, uint64, bytes memory);
    function penalize(uint32 id, uint256 bps, address reporter) external returns (uint256);
}

/// @title SwarmStorage: chain history and paid user storage, with shared audits (ADR 0009, 0014).
/// @notice Replaces HistoryRegistry at the same address and keeps its storage layout (new state
/// is appended), so the public testnet upgrades it by replacing the code at a hard fork.
///
/// History: the first block of every epoch records the previous epoch's index CID. Epochs older
/// than the recent window are kept by 16 validators and 16 storage providers without stake
/// (rendezvous hashing, computed by the node), paid from emission per audit pass.
///
/// User storage (deals): providers open an offer (capacity, minimum price per GiB and epoch);
/// a deal pays escrow for `replicas` copies of an IPLD block set for a number of epochs and is
/// assigned at random among the eligible offers. Providers claim per whole epoch. Each epoch the
/// node draws audit tasks for history and for deals; a certified failure penalises the provider
/// (1% of stake, or 10% of the bond and deactivation) and, for a deal, drops the copy without
/// paying its unclaimed epochs and assigns a replacement.
///
/// Storage providers without stake (ADR 0012) register for free (ids from 2^31 up); their
/// earnings build a bond first (20% of each payment, up to 64 BOLT).
contract SwarmStorage is SystemContract {
    enum State {
        Pending,
        Passed,
        Failed
    }

    enum Kind {
        History,
        Deal
    }

    struct Task {
        uint32 provider; // provider id expected to serve the data
        uint64 target; // History: epoch sampled; Deal: deal id
        uint64 height; // History: block sampled; Deal: block index in the deal
        State state;
        Kind kind; // appended (ADR 0014): zero for tasks recorded before
    }

    uint256 public constant AUDIT_SLASH_BPS = 100; // 1%
    /// First id of a storage provider without stake (validator ids stay below it).
    uint32 public constant STORAGE_ID_BASE = 2 ** 31;
    uint256 public constant BOND_TARGET = 64 ether;
    uint256 public constant BOND_BPS = 2_000;
    uint256 public constant BOND_SLASH_BPS = 1_000;
    uint64 public constant EXIT_DELAY = 14 days;

    struct StorageProvider {
        address account;
        bool active;
        uint64 exitAt; // bond withdrawable from then on (0: not exiting)
        uint256 bond;
    }
    /// Audit records older than this many epochs are deleted.
    uint64 public constant KEEP_EPOCHS = 32;

    // ------------------------------------------------------------------ deal parameters

    uint8 public constant MAX_REPLICAS = 16;
    uint64 public constant MAX_DEAL_EPOCHS = 3_650;
    /// Draw attempts per copy when assigning a deal.
    uint256 public constant DRAW_ATTEMPTS = 64;
    /// Unpaid deal value a provider without stake may hold: `COMMIT_BASE + COMMIT_BOND_MULT * bond`.
    uint256 public constant COMMIT_BASE = 5 ether;
    uint256 public constant COMMIT_BOND_MULT = 10;

    struct Offer {
        uint64 capacityMiB;
        uint64 usedMiB;
        uint128 minPrice; // wei per GiB per epoch
        bool open;
        bool listed; // in `offerIds`
        uint256 committed; // unpaid value of the copies it holds
    }

    struct Deal {
        address owner;
        uint8 replicas;
        bool closed;
        uint64 blocks;
        uint64 sizeMiB;
        uint64 startEpoch;
        uint64 endEpoch; // exclusive
        uint128 price; // wei per GiB per epoch
        uint128 perEpoch; // wei per copy per epoch
        uint256 escrow;
        uint64 size; // bytes
        bytes root; // DealIndex CID
    }

    struct Slot {
        uint32 provider;
        uint64 since; // first epoch paid (and audited from `since + 1`)
        uint64 paidThrough; // paid up to this epoch (exclusive)
        bool open;
    }

    // ------------------------------------------------------------------ state (HistoryRegistry layout)

    /// Epoch index CIDs (binary CID bytes).
    mapping(uint64 => bytes) internal indexes;
    /// Number of epochs recorded (they are recorded in order from 0).
    uint64 public indexedEpochs;
    /// libp2p peer id of each provider (for fetching its data).
    mapping(uint32 => bytes) public peerOf;
    mapping(uint64 => Task[]) internal tasks;
    mapping(uint64 => uint32[]) internal panels;
    /// Providers that passed a history audit, once per passed task.
    mapping(uint64 => uint32[]) internal passes;
    /// Storage providers without stake, by id (`STORAGE_ID_BASE + index`).
    mapping(uint32 => StorageProvider) internal storageProviders;
    /// Ids of all storage providers ever registered, in order.
    uint32[] internal storageIds;
    /// Signed-registration nonces.
    mapping(address => uint256) public nonces;

    // ------------------------------------------------------------------ state (ADR 0014, appended)

    mapping(uint32 => Offer) internal offers;
    uint32[] internal offerIds;
    mapping(uint256 => Deal) internal deals;
    mapping(uint256 => Slot[]) internal slots;
    uint256 public dealCount;
    /// Deals ever assigned to a provider (the node filters the ones still open).
    mapping(uint32 => uint256[]) internal providerDeals;
    /// Withdrawable payments and refunds.
    mapping(address => uint256) public balanceOf;

    event EpochIndexed(uint64 indexed epoch, bytes cid);
    event PeerSet(uint32 indexed id, bytes peerId);
    event AuditsDrawn(uint64 indexed epoch, uint256 tasks);
    event AuditRecorded(uint64 indexed epoch, uint16 task, uint32 provider, bool passed, uint256 penalty);
    event StorageRegistered(uint32 indexed id, address indexed account, bytes peerId);
    event StorageEarned(uint32 indexed id, uint256 paid, uint256 toBond);
    event StorageExit(uint32 indexed id, uint64 exitAt);
    event OfferSet(uint32 indexed id, uint64 capacityMiB, uint128 minPrice, bool open);
    event DealCreated(uint256 indexed id, address indexed owner, bytes root, uint64 blocks, uint64 size, uint8 replicas, uint64 startEpoch, uint64 endEpoch, uint128 price);
    event SlotAssigned(uint256 indexed id, uint256 slot, uint32 indexed provider, uint64 since);
    event SlotDropped(uint256 indexed id, uint256 slot, uint32 indexed provider, uint256 forfeited);
    event DealPaid(uint256 indexed id, uint256 slot, uint32 indexed provider, uint256 amount);
    event DealEnded(uint256 indexed id, uint64 endEpoch);
    event DealClosed(uint256 indexed id, uint256 refund);

    error NotOwner();
    error OutOfOrder();
    error BadTask();
    error Expired();
    error BadSignature();
    error NotStorageProvider();
    error NotExited();
    error TransferFailed();
    error BadDeal();
    error NoProviders();
    error NotLive();
    error Underpaid();
    error DealOver();

    /// First block of epoch `epoch + 1`: the index of epoch `epoch`.
    function recordEpoch(uint64 epoch, bytes calldata cid) external onlySystem {
        if (epoch != indexedEpochs) revert OutOfOrder();
        indexes[epoch] = cid;
        indexedEpochs = epoch + 1;
        emit EpochIndexed(epoch, cid);
    }

    function epochIndex(uint64 epoch) external view returns (bytes memory) {
        return indexes[epoch];
    }

    /// A validator's owner publishes the libp2p peer id its node serves data from.
    function setPeer(uint32 id, bytes calldata peerId) external {
        (address owner,,,,,) = IStakingHistory(Sys.STAKING).validator(id);
        if (msg.sender != owner) revert NotOwner();
        peerOf[id] = peerId;
        emit PeerSet(id, peerId);
    }

    // ------------------------------------------------------------------ providers without stake

    /// Registers the caller as a storage provider serving from libp2p peer `peerId`.
    function registerStorage(bytes calldata peerId) external returns (uint32) {
        return _registerStorage(msg.sender, peerId);
    }

    /// Registers `account` from its signature, submitted by anyone (it needs no BOLT).
    function registerStorageFor(address account, bytes calldata peerId, uint256 deadline, bytes calldata sig)
        external
        returns (uint32)
    {
        if (block.timestamp > deadline) revert Expired();
        if (sig.length != 65) revert BadSignature();
        bytes32 r = bytes32(sig[0:32]);
        bytes32 s_ = bytes32(sig[32:64]);
        uint8 v = uint8(sig[64]);
        if (v < 27) v += 27;
        if (uint256(s_) > 0x7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0) revert BadSignature();
        address got = ecrecover(registrationDigest(account, peerId, deadline), v, r, s_);
        if (got == address(0) || got != account) revert BadSignature();
        nonces[account]++;
        return _registerStorage(account, peerId);
    }

    /// EIP-191 personal-sign hash `account` signs to register (bound to chain, contract, nonce).
    function registrationDigest(address account, bytes calldata peerId, uint256 deadline)
        public
        view
        returns (bytes32)
    {
        bytes32 h = keccak256(
            abi.encode(block.chainid, address(this), keccak256("registerStorage"), account, nonces[account], peerId, deadline)
        );
        return keccak256(abi.encodePacked("\x19Ethereum Signed Message:\n32", h));
    }

    function _registerStorage(address account, bytes calldata peerId) internal returns (uint32 id) {
        id = STORAGE_ID_BASE + uint32(storageIds.length);
        storageIds.push(id);
        storageProviders[id] = StorageProvider({account: account, active: true, exitAt: 0, bond: 0});
        peerOf[id] = peerId;
        emit StorageRegistered(id, account, peerId);
        emit PeerSet(id, peerId);
    }

    /// Active storage providers without stake (the node assigns them history).
    function activeStorageProviders() external view returns (uint32[] memory ids) {
        uint256 n;
        for (uint256 i = 0; i < storageIds.length; i++) {
            if (storageProviders[storageIds[i]].active) n++;
        }
        ids = new uint32[](n);
        n = 0;
        for (uint256 i = 0; i < storageIds.length; i++) {
            if (storageProviders[storageIds[i]].active) ids[n++] = storageIds[i];
        }
    }

    function storageProvider(uint32 id)
        external
        view
        returns (address account, bool active, uint64 exitAt, uint256 bond)
    {
        StorageProvider storage p = storageProviders[id];
        return (p.account, p.active, p.exitAt, p.bond);
    }

    /// Ids registered to `account` (active or not).
    function storageIdsOf(address account) external view returns (uint32[] memory ids) {
        uint256 n;
        for (uint256 i = 0; i < storageIds.length; i++) {
            if (storageProviders[storageIds[i]].account == account) n++;
        }
        ids = new uint32[](n);
        n = 0;
        for (uint256 i = 0; i < storageIds.length; i++) {
            if (storageProviders[storageIds[i]].account == account) ids[n++] = storageIds[i];
        }
    }

    /// The provider's account moves its node to another peer id.
    function setStoragePeer(uint32 id, bytes calldata peerId) external {
        if (storageProviders[id].account != msg.sender) revert NotOwner();
        peerOf[id] = peerId;
        emit PeerSet(id, peerId);
    }

    /// Stops receiving assignments; the bond is withdrawable after `EXIT_DELAY`. Deal copies it
    /// still holds become repairable (and are no longer paid).
    function exitStorage(uint32 id) external {
        StorageProvider storage p = storageProviders[id];
        if (p.account != msg.sender) revert NotOwner();
        p.active = false;
        p.exitAt = uint64(block.timestamp) + EXIT_DELAY;
        emit StorageExit(id, p.exitAt);
    }

    /// Returns an exited (or deactivated) provider's remaining bond to its account.
    function releaseStorageBond(uint32 id) external {
        StorageProvider storage p = storageProviders[id];
        if (p.active || p.exitAt == 0 || block.timestamp < p.exitAt) revert NotExited();
        uint256 amount = p.bond;
        p.bond = 0;
        (bool ok,) = p.account.call{value: amount}("");
        if (!ok) revert TransferFailed();
    }

    /// Adds to a storage provider's bond (raises how much deal value it may hold).
    function depositBond(uint32 id) external payable {
        StorageProvider storage p = storageProviders[id];
        if (p.account == address(0) || !p.active) revert NotStorageProvider();
        p.bond += msg.value;
    }

    /// RewardDistributor forwards a storage provider's claimed rewards: 20% goes to the bond until
    /// it reaches 64 BOLT, the rest to the account.
    function depositEarnings(uint32 id) external payable {
        if (msg.sender != Sys.REWARDS) revert Unauthorized();
        StorageProvider storage p = storageProviders[id];
        if (p.account == address(0)) revert NotStorageProvider();
        uint256 toBond = _toBond(p, msg.value);
        uint256 paid = msg.value - toBond;
        emit StorageEarned(id, paid, toBond);
        (bool ok,) = p.account.call{value: paid}("");
        if (!ok) revert TransferFailed();
    }

    function _toBond(StorageProvider storage p, uint256 amount) internal returns (uint256 toBond) {
        if (p.bond < BOND_TARGET) {
            toBond = amount * BOND_BPS / 10_000;
            if (p.bond + toBond > BOND_TARGET) toBond = BOND_TARGET - p.bond;
            p.bond += toBond;
        }
    }

    // ------------------------------------------------------------------ provider helpers

    /// Account controlling provider `id` (validator owner, or storage account).
    function _account(uint32 id) internal view returns (address) {
        if (id >= STORAGE_ID_BASE) return storageProviders[id].account;
        (address owner,,,,,) = IStakingHistory(Sys.STAKING).validator(id);
        return owner;
    }

    /// Whether provider `id` is still in the registry: an active storage provider, or an active
    /// validator.
    function _live(uint32 id) internal view returns (bool) {
        if (id >= STORAGE_ID_BASE) return storageProviders[id].active;
        (,,, uint8 status,,) = IStakingHistory(Sys.STAKING).validator(id);
        return status == 1;
    }

    function _epoch() internal view returns (uint64) {
        (bool ok, bytes memory r) = Sys.CONSENSUS.staticcall(abi.encodeWithSignature("currentEpoch()"));
        if (!ok || r.length < 32) return 0;
        return abi.decode(r, (uint64));
    }

    // ------------------------------------------------------------------ offers

    /// Opens (or updates) provider `id`'s offer for user storage.
    function offerStorage(uint32 id, uint64 capacityMiB, uint128 minPrice) external {
        if (msg.sender != _account(id) || msg.sender == address(0)) revert NotOwner();
        if (!_live(id)) revert NotLive();
        Offer storage o = offers[id];
        o.capacityMiB = capacityMiB;
        o.minPrice = minPrice;
        o.open = true;
        if (!o.listed) {
            o.listed = true;
            offerIds.push(id);
        }
        emit OfferSet(id, capacityMiB, minPrice, true);
    }

    /// Stops taking new deals (copies already held stay).
    function closeOffer(uint32 id) external {
        if (msg.sender != _account(id)) revert NotOwner();
        Offer storage o = offers[id];
        o.open = false;
        emit OfferSet(id, o.capacityMiB, o.minPrice, false);
    }

    function offer(uint32 id)
        external
        view
        returns (uint64 capacityMiB, uint64 usedMiB, uint128 minPrice, bool open, uint256 committed)
    {
        Offer storage o = offers[id];
        return (o.capacityMiB, o.usedMiB, o.minPrice, o.open, o.committed);
    }

    /// Every provider that ever opened an offer.
    function offerList() external view returns (uint32[] memory) {
        return offerIds;
    }

    // ------------------------------------------------------------------ deals

    /// Pays `replicas` copies of the blocks listed by DealIndex `root` (`blocks` blocks, `size`
    /// bytes) for `epochs` epochs from the current one, at `price` wei per GiB per epoch. The
    /// escrow is `ceil(price * MiB / 1024) * replicas * epochs`; any excess is refunded at the end.
    function createDeal(bytes calldata root, uint64 blocks, uint64 size, uint8 replicas, uint64 epochs, uint128 price)
        external
        payable
        returns (uint256 id)
    {
        if (
            root.length == 0 || blocks == 0 || size == 0 || replicas == 0 || replicas > MAX_REPLICAS || epochs == 0
                || epochs > MAX_DEAL_EPOCHS || price == 0
        ) revert BadDeal();
        uint64 mib = uint64((uint256(size) + (1 << 20) - 1) >> 20);
        uint256 perEpoch = (uint256(price) * mib + 1023) / 1024;
        if (perEpoch > type(uint128).max) revert BadDeal();
        uint256 cost = perEpoch * replicas * epochs;
        if (msg.value < cost) revert Underpaid();
        uint64 start = _epoch();
        id = dealCount++;
        Deal storage d = deals[id];
        d.owner = msg.sender;
        d.replicas = replicas;
        d.blocks = blocks;
        d.sizeMiB = mib;
        d.size = size;
        d.startEpoch = start;
        d.endEpoch = start + epochs;
        d.price = price;
        d.perEpoch = uint128(perEpoch);
        d.escrow = msg.value;
        d.root = root;
        emit DealCreated(id, msg.sender, root, blocks, size, replicas, start, start + epochs, price);
        for (uint256 i = 0; i < replicas; i++) {
            (bool ok, uint32 who) = _draw(id, d, start, i);
            if (!ok) revert NoProviders();
            _assign(id, d, slots[id].length, who, start, true);
        }
    }

    /// Adds `epochs` epochs (paying for them now).
    function extendDeal(uint256 id, uint64 epochs) external payable {
        Deal storage d = deals[id];
        if (d.owner != msg.sender) revert NotOwner();
        if (d.closed || _epoch() >= d.endEpoch) revert DealOver();
        uint256 total = uint256(d.endEpoch - d.startEpoch) + epochs;
        if (epochs == 0 || total > MAX_DEAL_EPOCHS) revert BadDeal();
        if (msg.value < uint256(d.perEpoch) * d.replicas * epochs) revert Underpaid();
        d.endEpoch += epochs;
        d.escrow += msg.value;
        Slot[] storage ss = slots[id];
        for (uint256 i = 0; i < ss.length; i++) {
            if (ss[i].open) offers[ss[i].provider].committed += uint256(d.perEpoch) * epochs;
        }
        emit DealEnded(id, d.endEpoch);
    }

    /// Ends the deal after the current epoch (which is still paid); the rest is refunded on close.
    function cancelDeal(uint256 id) external {
        Deal storage d = deals[id];
        if (d.owner != msg.sender) revert NotOwner();
        uint64 end = _epoch() + 1;
        if (d.closed || end >= d.endEpoch) revert DealOver();
        uint64 cut = d.endEpoch - end;
        Slot[] storage ss = slots[id];
        for (uint256 i = 0; i < ss.length; i++) {
            if (ss[i].open) _uncommit(ss[i].provider, uint256(d.perEpoch) * cut);
        }
        d.endEpoch = end;
        emit DealEnded(id, end);
    }

    /// Pays copy `slot` of deal `id` for its whole epochs up to now (anyone may call).
    function claim(uint256 id, uint256 slot) external {
        Deal storage d = deals[id];
        Slot storage s = slots[id][slot];
        if (!s.open || d.closed) revert BadDeal();
        if (!_live(s.provider)) revert NotLive();
        _pay(id, d, slot, _epoch());
    }

    /// After the end: pays every live copy up to the end, drops the rest, refunds the owner.
    function closeDeal(uint256 id) external {
        Deal storage d = deals[id];
        if (d.owner == address(0) || d.closed) revert BadDeal();
        if (_epoch() < d.endEpoch) revert BadDeal();
        Slot[] storage ss = slots[id];
        for (uint256 i = 0; i < ss.length; i++) {
            if (!ss[i].open) continue;
            if (_live(ss[i].provider)) _pay(id, d, i, d.endEpoch);
            _drop(id, d, i);
        }
        d.closed = true;
        uint256 refund = d.escrow;
        d.escrow = 0;
        balanceOf[d.owner] += refund;
        emit DealClosed(id, refund);
    }

    /// Replaces copy `slot` if its provider left the registry (unpaid epochs are forfeited), or
    /// fills it if it is empty. Anyone may call.
    function repair(uint256 id, uint256 slot) external {
        Deal storage d = deals[id];
        uint64 now_ = _epoch();
        if (d.closed || now_ >= d.endEpoch) revert DealOver();
        Slot storage s = slots[id][slot];
        if (s.open) {
            if (_live(s.provider)) revert BadDeal();
            _drop(id, d, slot);
        }
        (bool ok, uint32 who) = _draw(id, d, now_, uint256(keccak256(abi.encode(slot, block.number))));
        if (!ok) revert NoProviders();
        _assign(id, d, slot, who, now_, false);
    }

    function _pay(uint256 id, Deal storage d, uint256 slot, uint64 upTo) internal {
        Slot storage s = slots[id][slot];
        uint64 to = upTo < d.endEpoch ? upTo : d.endEpoch;
        if (to <= s.paidThrough) return;
        uint256 amount = uint256(d.perEpoch) * (to - s.paidThrough);
        s.paidThrough = to;
        d.escrow -= amount;
        _uncommit(s.provider, amount);
        _credit(s.provider, amount);
        emit DealPaid(id, slot, s.provider, amount);
    }

    /// Credits a provider: storage providers without stake build their bond first.
    function _credit(uint32 p, uint256 amount) internal {
        if (p >= STORAGE_ID_BASE) {
            StorageProvider storage sp = storageProviders[p];
            uint256 toBond = _toBond(sp, amount);
            balanceOf[sp.account] += amount - toBond;
            emit StorageEarned(p, amount - toBond, toBond);
        } else {
            balanceOf[_account(p)] += amount;
        }
    }

    function _uncommit(uint32 p, uint256 amount) internal {
        Offer storage o = offers[p];
        o.committed = o.committed > amount ? o.committed - amount : 0;
    }

    /// Drops copy `slot` without paying its unclaimed epochs (they stay in escrow).
    function _drop(uint256 id, Deal storage d, uint256 slot) internal {
        Slot storage s = slots[id][slot];
        s.open = false;
        Offer storage o = offers[s.provider];
        o.usedMiB = o.usedMiB > d.sizeMiB ? o.usedMiB - d.sizeMiB : 0;
        uint256 forfeited;
        if (d.endEpoch > s.paidThrough) forfeited = uint256(d.perEpoch) * (d.endEpoch - s.paidThrough);
        _uncommit(s.provider, forfeited);
        emit SlotDropped(id, slot, s.provider, forfeited);
    }

    function _assign(uint256 id, Deal storage d, uint256 slot, uint32 who, uint64 since, bool push) internal {
        Slot memory s = Slot({provider: who, since: since, paidThrough: since, open: true});
        if (push) slots[id].push(s);
        else slots[id][slot] = s;
        Offer storage o = offers[who];
        o.usedMiB += d.sizeMiB;
        o.committed += uint256(d.perEpoch) * (d.endEpoch - since);
        providerDeals[who].push(id);
        emit SlotAssigned(id, slot, who, since);
    }

    /// Random eligible offer for a copy of deal `id` from epoch `since`.
    function _draw(uint256 id, Deal storage d, uint64 since, uint256 salt) internal view returns (bool, uint32) {
        uint256 n = offerIds.length;
        if (n == 0) return (false, 0);
        uint256 value = uint256(d.perEpoch) * (d.endEpoch - since);
        for (uint256 t = 0; t < DRAW_ATTEMPTS; t++) {
            uint32 c = offerIds[uint256(keccak256(abi.encode(block.prevrandao, id, salt, t))) % n];
            if (_eligible(id, d, c, value)) return (true, c);
        }
        return (false, 0);
    }

    function _eligible(uint256 id, Deal storage d, uint32 c, uint256 value) internal view returns (bool) {
        Offer storage o = offers[c];
        if (!o.open || o.minPrice > d.price || o.capacityMiB < o.usedMiB + d.sizeMiB) return false;
        if (!_live(c)) return false;
        if (c >= STORAGE_ID_BASE && o.committed + value > COMMIT_BASE + COMMIT_BOND_MULT * storageProviders[c].bond) {
            return false;
        }
        Slot[] storage ss = slots[id];
        for (uint256 i = 0; i < ss.length; i++) {
            if (ss[i].open && ss[i].provider == c) return false;
        }
        return true;
    }

    function deal(uint256 id)
        external
        view
        returns (
            address owner,
            bytes memory root,
            uint64 blocks,
            uint64 size,
            uint8 replicas,
            uint64 startEpoch,
            uint64 endEpoch,
            uint128 price,
            uint128 perEpoch,
            uint256 escrow,
            bool closed
        )
    {
        Deal storage d = deals[id];
        return (
            d.owner, d.root, d.blocks, d.size, d.replicas, d.startEpoch, d.endEpoch, d.price, d.perEpoch, d.escrow, d.closed
        );
    }

    /// Copies of deal `id` (slot order).
    function dealSlots(uint256 id)
        external
        view
        returns (uint32[] memory providers, uint64[] memory since, uint64[] memory paidThrough, bool[] memory open)
    {
        Slot[] storage ss = slots[id];
        uint256 n = ss.length;
        providers = new uint32[](n);
        since = new uint64[](n);
        paidThrough = new uint64[](n);
        open = new bool[](n);
        for (uint256 i = 0; i < n; i++) {
            providers[i] = ss[i].provider;
            since[i] = ss[i].since;
            paidThrough[i] = ss[i].paidThrough;
            open[i] = ss[i].open;
        }
    }

    /// Deals ever assigned to provider `id` (repeats possible; check `dealSlots`).
    function dealsOf(uint32 id) external view returns (uint256[] memory) {
        return providerDeals[id];
    }

    /// Sends `msg.sender`'s balance to `to`.
    function withdraw(address to) external {
        uint256 amount = balanceOf[msg.sender];
        balanceOf[msg.sender] = 0;
        (bool ok,) = to.call{value: amount}("");
        if (!ok) revert TransferFailed();
    }

    /// Pushes `account`'s balance to it (anyone may call).
    function payout(address account) external {
        uint256 amount = balanceOf[account];
        balanceOf[account] = 0;
        (bool ok,) = account.call{value: amount}("");
        if (!ok) revert TransferFailed();
    }

    // ------------------------------------------------------------------ audits

    /// First block of epoch `epoch`: the audit panel (committee members) and the history tasks.
    function beginAudits(
        uint64 epoch,
        uint32[] calldata panel,
        uint32[] calldata providers,
        uint64[] calldata targets,
        uint64[] calldata heights
    ) external onlySystem {
        if (providers.length != targets.length || targets.length != heights.length) revert BadTask();
        panels[epoch] = panel;
        Task[] storage t = tasks[epoch];
        for (uint256 i = 0; i < providers.length; i++) {
            t.push(
                Task({
                    provider: providers[i],
                    target: targets[i],
                    height: heights[i],
                    state: State.Pending,
                    kind: Kind.History
                })
            );
        }
        if (epoch >= KEEP_EPOCHS) {
            uint64 old = epoch - KEEP_EPOCHS;
            delete tasks[old];
            delete panels[old];
            delete passes[old];
        }
        emit AuditsDrawn(epoch, providers.length);
    }

    /// Right after `beginAudits`: the deal tasks (provider, deal, block index), appended.
    function addDealAudits(
        uint64 epoch,
        uint32[] calldata providers,
        uint64[] calldata dealIds,
        uint64[] calldata indexes_
    ) external onlySystem {
        if (providers.length != dealIds.length || dealIds.length != indexes_.length) revert BadTask();
        Task[] storage t = tasks[epoch];
        for (uint256 i = 0; i < providers.length; i++) {
            t.push(
                Task({provider: providers[i], target: dealIds[i], height: indexes_[i], state: State.Pending, kind: Kind.Deal})
            );
        }
        emit AuditsDrawn(epoch, t.length);
    }

    /// A certified audit result (checked by the node against the panel's keys). Returns false if
    /// the task was already decided.
    function recordAudit(uint64 epoch, uint16 task, bool ok) external onlySystem returns (bool) {
        Task[] storage ts = tasks[epoch];
        if (task >= ts.length) revert BadTask();
        Task storage t = ts[task];
        if (t.state != State.Pending) return false;
        uint256 penalty;
        if (ok) {
            t.state = State.Passed;
            if (t.kind == Kind.History) passes[epoch].push(t.provider);
        } else {
            t.state = State.Failed;
            penalty = _penalize(t.provider);
            if (t.kind == Kind.Deal) _failCopy(t.target, t.provider);
        }
        emit AuditRecorded(epoch, task, t.provider, ok, penalty);
        return true;
    }

    function _penalize(uint32 provider) internal returns (uint256 penalty) {
        if (provider >= STORAGE_ID_BASE) {
            // Without stake: burn 10% of the bond (it stays locked here) and stop assigning.
            StorageProvider storage sp = storageProviders[provider];
            penalty = sp.bond * BOND_SLASH_BPS / 10_000;
            sp.bond -= penalty;
            if (sp.active) {
                sp.active = false;
                sp.exitAt = uint64(block.timestamp) + EXIT_DELAY;
                emit StorageExit(provider, sp.exitAt);
            }
            if (penalty > 0) IRewardsBurn(Sys.REWARDS).burnSupply(penalty);
        } else {
            try IStakingHistory(Sys.STAKING).penalize(provider, AUDIT_SLASH_BPS, block.coinbase) returns (uint256 p) {
                penalty = p;
            } catch {}
        }
    }

    /// A failed deal audit: drop the provider's copy and try to assign a replacement.
    function _failCopy(uint64 id, uint32 provider) internal {
        Deal storage d = deals[id];
        if (d.closed) return;
        Slot[] storage ss = slots[id];
        uint64 now_ = _epoch();
        for (uint256 i = 0; i < ss.length; i++) {
            if (!ss[i].open || ss[i].provider != provider) continue;
            _drop(id, d, i);
            if (now_ < d.endEpoch) {
                (bool found, uint32 who) = _draw(id, d, now_, uint256(keccak256(abi.encode(i, block.number))));
                if (found) _assign(id, d, i, who, now_, false);
            }
            return;
        }
    }

    /// Panel, tasks and their states for `epoch`.
    function audits(uint64 epoch)
        external
        view
        returns (
            uint32[] memory panel,
            uint32[] memory providers,
            uint64[] memory targets,
            uint64[] memory heights,
            uint8[] memory states
        )
    {
        Task[] storage ts = tasks[epoch];
        uint256 n = ts.length;
        providers = new uint32[](n);
        targets = new uint64[](n);
        heights = new uint64[](n);
        states = new uint8[](n);
        for (uint256 i = 0; i < n; i++) {
            providers[i] = ts[i].provider;
            targets[i] = ts[i].target;
            heights[i] = ts[i].height;
            states[i] = uint8(ts[i].state);
        }
        return (panels[epoch], providers, targets, heights, states);
    }

    /// Kind of each task of `epoch` (0: history, 1: deal).
    function taskKinds(uint64 epoch) external view returns (uint8[] memory kinds) {
        Task[] storage ts = tasks[epoch];
        kinds = new uint8[](ts.length);
        for (uint256 i = 0; i < ts.length; i++) {
            kinds[i] = uint8(ts[i].kind);
        }
    }

    /// Providers that passed history audits in `epoch` (one entry per passed task).
    function passed(uint64 epoch) external view returns (uint32[] memory) {
        return passes[epoch];
    }
}
