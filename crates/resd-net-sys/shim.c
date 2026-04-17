#include <rte_errno.h>
#include <rte_ethdev.h>
#include <rte_mbuf.h>

int resd_rte_errno(void) {
    return rte_errno;
}

/* The burst helpers and rte_pktmbuf_free are `static inline` in DPDK
 * headers, so bindgen does not emit FFI stubs for them. Expose real
 * extern wrappers here so the Rust hot path can call them directly.
 */
uint16_t resd_rte_eth_rx_burst(uint16_t port_id, uint16_t queue_id,
                               struct rte_mbuf **rx_pkts, uint16_t nb_pkts) {
    return rte_eth_rx_burst(port_id, queue_id, rx_pkts, nb_pkts);
}

uint16_t resd_rte_eth_tx_burst(uint16_t port_id, uint16_t queue_id,
                               struct rte_mbuf **tx_pkts, uint16_t nb_pkts) {
    return rte_eth_tx_burst(port_id, queue_id, tx_pkts, nb_pkts);
}

void resd_rte_pktmbuf_free(struct rte_mbuf *m) {
    rte_pktmbuf_free(m);
}
