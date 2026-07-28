#include <srt/srt.h>

#include <arpa/inet.h>
#include <stdbool.h>
#include <stdint.h>
#include <string.h>

enum {
    RUSHLS_SRT_RECV_ERROR = -1,
    RUSHLS_SRT_RECV_RETRY = -2,
};

typedef struct RushlsSrtOptions {
    int32_t latency_ms;
    int32_t peer_idle_timeout_ms;
    int32_t receive_buffer_bytes;
    int32_t payload_size;
    int32_t minimum_peer_version;
    const char *passphrase;
    int32_t passphrase_length;
    int32_t key_length;
} RushlsSrtOptions;

typedef struct RushlsSrtPeer {
    uint8_t address[16];
    uint16_t port;
    uint8_t is_ipv6;
    uint32_t scope_id;
    uint32_t protocol_version;
    char stream_id[513];
    int32_t stream_id_length;
} RushlsSrtPeer;

static int set_flag(SRTSOCKET socket, SRT_SOCKOPT option, const void *value, int length) {
    return srt_setsockflag(socket, option, value, length);
}

static int configure_socket(SRTSOCKET socket, const RushlsSrtOptions *options, bool receiver) {
    const int transmission_type = SRTT_LIVE;
    const int enabled = 1;

    if (set_flag(socket, SRTO_TRANSTYPE, &transmission_type, sizeof(transmission_type)) == SRT_ERROR ||
        set_flag(socket, SRTO_MESSAGEAPI, &enabled, sizeof(enabled)) == SRT_ERROR ||
        set_flag(socket, SRTO_TSBPDMODE, &enabled, sizeof(enabled)) == SRT_ERROR ||
        set_flag(socket, receiver ? SRTO_RCVSYN : SRTO_SNDSYN, &enabled, sizeof(enabled)) ==
            SRT_ERROR ||
        set_flag(socket, SRTO_LATENCY, &options->latency_ms, sizeof(options->latency_ms)) == SRT_ERROR ||
        set_flag(socket, SRTO_PEERIDLETIMEO, &options->peer_idle_timeout_ms,
                 sizeof(options->peer_idle_timeout_ms)) == SRT_ERROR ||
        set_flag(socket, SRTO_PAYLOADSIZE, &options->payload_size, sizeof(options->payload_size)) ==
            SRT_ERROR ||
        set_flag(socket, SRTO_MINVERSION, &options->minimum_peer_version,
                 sizeof(options->minimum_peer_version)) == SRT_ERROR) {
        return SRT_ERROR;
    }

    if (receiver &&
        set_flag(socket, SRTO_RCVBUF, &options->receive_buffer_bytes,
                 sizeof(options->receive_buffer_bytes)) == SRT_ERROR) {
        return SRT_ERROR;
    }

    if (options->passphrase_length > 0) {
        const int enforced = 1;
        if (set_flag(socket, SRTO_ENFORCEDENCRYPTION, &enforced, sizeof(enforced)) == SRT_ERROR ||
            set_flag(socket, SRTO_PASSPHRASE, options->passphrase,
                     options->passphrase_length) == SRT_ERROR ||
            set_flag(socket, SRTO_PBKEYLEN, &options->key_length, sizeof(options->key_length)) ==
                SRT_ERROR) {
            return SRT_ERROR;
        }
    }

    return 0;
}

static int copy_address(const struct sockaddr *address, RushlsSrtPeer *peer) {
    memset(peer->address, 0, sizeof(peer->address));
    peer->scope_id = 0;

    if (address->sa_family == AF_INET) {
        const struct sockaddr_in *ipv4 = (const struct sockaddr_in *)address;
        memcpy(peer->address, &ipv4->sin_addr, sizeof(ipv4->sin_addr));
        peer->port = ntohs(ipv4->sin_port);
        peer->is_ipv6 = 0;
        return 0;
    }
    if (address->sa_family == AF_INET6) {
        const struct sockaddr_in6 *ipv6 = (const struct sockaddr_in6 *)address;
        memcpy(peer->address, &ipv6->sin6_addr, sizeof(ipv6->sin6_addr));
        peer->port = ntohs(ipv6->sin6_port);
        peer->is_ipv6 = 1;
        peer->scope_id = ipv6->sin6_scope_id;
        return 0;
    }
    return SRT_ERROR;
}

int rushls_srt_startup(void) {
    return srt_startup();
}

int rushls_srt_cleanup(void) {
    return srt_cleanup();
}

uint32_t rushls_srt_version(void) {
    return srt_getversion();
}

const char *rushls_srt_last_error(void) {
    return srt_getlasterror_str();
}

int rushls_srt_listener_open(const struct sockaddr *address, int address_length,
                             const RushlsSrtOptions *options, int backlog,
                             int32_t *listener, int32_t *poll, RushlsSrtPeer *local) {
    SRTSOCKET socket = srt_create_socket();
    if (socket == SRT_INVALID_SOCK) {
        return SRT_ERROR;
    }
    const int dual_stack = 0;
    if (configure_socket(socket, options, true) == SRT_ERROR ||
        // libSRT defaults wildcard IPv6 listeners to IPv6-only and rejects
        // binding :: on macOS unless this option is set explicitly.
        (address->sa_family == AF_INET6 &&
         set_flag(socket, SRTO_IPV6ONLY, &dual_stack, sizeof(dual_stack)) == SRT_ERROR) ||
        srt_bind(socket, address, address_length) == SRT_ERROR ||
        srt_listen(socket, backlog) == SRT_ERROR) {
        srt_close(socket);
        return SRT_ERROR;
    }

    const int events = SRT_EPOLL_IN | SRT_EPOLL_ERR;
    const int poll_id = srt_epoll_create();
    if (poll_id == SRT_ERROR || srt_epoll_add_usock(poll_id, socket, &events) == SRT_ERROR) {
        if (poll_id != SRT_ERROR) {
            srt_epoll_release(poll_id);
        }
        srt_close(socket);
        return SRT_ERROR;
    }

    struct sockaddr_storage bound;
    int bound_length = sizeof(bound);
    if (srt_getsockname(socket, (struct sockaddr *)&bound, &bound_length) == SRT_ERROR ||
        copy_address((const struct sockaddr *)&bound, local) == SRT_ERROR) {
        srt_epoll_release(poll_id);
        srt_close(socket);
        return SRT_ERROR;
    }

    *listener = socket;
    *poll = poll_id;
    return 0;
}

int rushls_srt_listener_wait(int32_t poll, int64_t timeout_ms) {
    SRT_EPOLL_EVENT event;
    const int count = srt_epoll_uwait(poll, &event, 1, timeout_ms);
    if (count <= 0) {
        return count;
    }
    return (event.events & SRT_EPOLL_IN) != 0 ? 1 : SRT_ERROR;
}

int rushls_srt_poll_close(int32_t poll) {
    return srt_epoll_release(poll);
}

int rushls_srt_accept(int32_t listener, int32_t *accepted, RushlsSrtPeer *peer) {
    struct sockaddr_storage address;
    int address_length = sizeof(address);
    SRTSOCKET socket = srt_accept(listener, (struct sockaddr *)&address, &address_length);
    if (socket == SRT_INVALID_SOCK) {
        return SRT_ERROR;
    }

    memset(peer, 0, sizeof(*peer));
    if (copy_address((const struct sockaddr *)&address, peer) == SRT_ERROR) {
        srt_close(socket);
        return SRT_ERROR;
    }

    const int asynchronous = 0;
    if (set_flag(socket, SRTO_RCVSYN, &asynchronous, sizeof(asynchronous)) == SRT_ERROR) {
        srt_close(socket);
        return SRT_ERROR;
    }

    int stream_id_length = 512;
    if (srt_getsockflag(socket, SRTO_STREAMID, peer->stream_id, &stream_id_length) == SRT_ERROR ||
        stream_id_length < 0 || stream_id_length > 512) {
        srt_close(socket);
        return SRT_ERROR;
    }
    peer->stream_id[stream_id_length] = '\0';
    peer->stream_id_length = stream_id_length;

    int version_length = sizeof(peer->protocol_version);
    if (srt_getsockflag(socket, SRTO_PEERVERSION, &peer->protocol_version, &version_length) ==
        SRT_ERROR) {
        srt_close(socket);
        return SRT_ERROR;
    }

    *accepted = socket;
    return 0;
}

int rushls_srt_recv(int32_t socket, uint8_t *buffer, int length) {
    const int received = srt_recvmsg2(socket, (char *)buffer, length, NULL);
    if (received >= 0) {
        return received;
    }

    int system_error = 0;
    const int error = srt_getlasterror(&system_error);
    if (error == SRT_EASYNCRCV || error == SRT_ETIMEOUT) {
        return RUSHLS_SRT_RECV_RETRY;
    }
    return RUSHLS_SRT_RECV_ERROR;
}

int rushls_srt_socket_state(int32_t socket) {
    return srt_getsockstate(socket);
}

int64_t rushls_srt_receive_loss_total(int32_t socket) {
    SRT_TRACEBSTATS statistics;
    if (srt_bstats(socket, &statistics, 0) == SRT_ERROR) {
        return SRT_ERROR;
    }
    return statistics.pktRcvLossTotal;
}

int rushls_srt_close(int32_t socket) {
    return srt_close(socket);
}

// The caller helpers keep loopback coverage independent of external SRT tools.
// They are declared from Rust only in tests, but compiling them here also
// continuously checks that our listener options remain valid libSRT API.
int rushls_srt_test_connect(const struct sockaddr *address, int address_length,
                            const RushlsSrtOptions *options, const char *stream_id,
                            int stream_id_length, int32_t *connected) {
    SRTSOCKET socket = srt_create_socket();
    if (socket == SRT_INVALID_SOCK) {
        return SRT_ERROR;
    }
    if (configure_socket(socket, options, false) == SRT_ERROR ||
        set_flag(socket, SRTO_STREAMID, stream_id, stream_id_length) == SRT_ERROR ||
        srt_connect(socket, address, address_length) == SRT_ERROR) {
        srt_close(socket);
        return SRT_ERROR;
    }
    *connected = socket;
    return 0;
}

int rushls_srt_test_send(int32_t socket, const uint8_t *buffer, int length) {
    return srt_sendmsg2(socket, (const char *)buffer, length, NULL);
}
