package main

import (
    "context"
    "crypto/rand"
    "fmt"
    "io"
    "net/http"

    "github.com/libp2p/go-libp2p/core/crypto"
    "github.com/libp2p/go-libp2p/core/peer"
    "github.com/libp2p/go-libp2p/core/transport"
    "github.com/libp2p/go-libp2p/p2p/transport/quicreuse"
    webtransport "github.com/libp2p/go-libp2p/p2p/transport/webtransport"
    "github.com/multiformats/go-multiaddr"
)

// This provides a way for test cases to discover the WebTransport address
func addrReporter(ma multiaddr.Multiaddr) {
    http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
        h := w.Header()
        h.Add("Access-Control-Allow-Origin", "*")
        h.Add("Cross-Origin-Resource-Policy", "cross-origin")
        h.Add("Content-Type", "text/plain; charset=utf-8")

        fmt.Fprint(w, ma.String())
    })

    http.ListenAndServe(":4455", nil)
}

func serveConn(conn transport.CapableConn) {
    go func() {
        for {
            stream, err := conn.OpenStream(context.Background())
            if err != nil {
                break;
            }

            go io.Copy(stream, stream)
        }
    }()

    for {
        stream, err := conn.AcceptStream()
        if err != nil {
            break
        }

        go io.Copy(stream, stream)
    }
}

func main() {
    priv, pub, err := crypto.GenerateEd25519Key(rand.Reader)
    if err != nil {
        panic(err)
    }

    peerId, err := peer.IDFromPublicKey(pub)
    if err != nil {
        panic(err)
    }

    connManager, err := quicreuse.NewConnManager([32]byte{})
    if err != nil {
        panic(err)
    }

    transport, err := webtransport.New(priv, nil, connManager, nil, nil);
    if err != nil {
        panic(err)
    }

    listener, err := transport.Listen(multiaddr.StringCast("/ip4/127.0.0.1/udp/4466/quic-v1/webtransport"))
    if err != nil {
        panic(err)
    }

    addr := listener.Multiaddr().Encapsulate(multiaddr.StringCast("/p2p/" + peerId.String()))

    go addrReporter(addr)

    for {
        conn, err := listener.Accept()
        if err != nil {
            panic(nil)
        }

        go serveConn(conn)
    }
}
