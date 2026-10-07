//! Network (NetDll_*) imports. The runtime has no network: the console reports no Ethernet link,
//! so titles take their offline paths.

use super::ImportCall;

/// XNetGetEthernetLinkStatus: no flags set means no cable is connected.
const NO_ETHERNET_LINK: u32 = 0;

/// XNetGetTitleXnAddr: no address could be obtained.
const XNADDR_NONE: u32 = 0x1;
/// XNADDR: IP addresses, online port, Ethernet address and online identifiers.
const XNADDR_SIZE: usize = 36;

/// NetDll_XNetGetTitleXnAddr(caller, address out) -> how the address was obtained.
pub(super) fn xnet_get_title_xn_addr(call: &ImportCall) -> u32 {
    call.memory.write_bytes(call.argument(1), &[0; XNADDR_SIZE]);
    XNADDR_NONE
}

/// NetDll_XNetGetEthernetLinkStatus(caller) -> link flags.
pub(super) fn xnet_get_ethernet_link_status(_: &ImportCall) -> u32 {
    NO_ETHERNET_LINK
}
