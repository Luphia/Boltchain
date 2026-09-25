// SPDX-License-Identifier: MIT
pragma solidity 0.8.17;

import {ERC20} from "solmate/src/tokens/ERC20.sol";
import {SafeTransferLib} from "solmate/src/utils/SafeTransferLib.sol";

/// @notice Wrapped BOLT, WETH9-compatible (deposit, withdraw, receive).
contract WBOLT is ERC20("Wrapped BOLT", "WBOLT", 18) {
    using SafeTransferLib for address;

    event Deposit(address indexed from, uint256 amount);
    event Withdrawal(address indexed to, uint256 amount);

    function deposit() public payable {
        _mint(msg.sender, msg.value);
        emit Deposit(msg.sender, msg.value);
    }

    function withdraw(uint256 amount) public {
        _burn(msg.sender, amount);
        emit Withdrawal(msg.sender, amount);
        msg.sender.safeTransferETH(amount);
    }

    receive() external payable {
        deposit();
    }
}

/// @notice Test token anyone can mint, for testnet pools only (no value).
contract TestToken is ERC20 {
    constructor(string memory name_, string memory symbol_, uint8 decimals_) ERC20(name_, symbol_, decimals_) {}

    function mint(address to, uint256 amount) external {
        require(amount <= 1_000_000 * 10 ** decimals, "at most 1,000,000 per call");
        _mint(to, amount);
    }
}
