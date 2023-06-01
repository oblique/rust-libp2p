use libp2p_core::{InboundUpgrade, OutboundUpgrade};
use libp2p_identity as identity;
use libp2p_noise as noise;
use multihash::{Code, Multihash, MultihashDigest};

// `valid_certhases` must be a strict subset of `server_certhashes`
fn validate_webtransport_certhashes(
    valid_certhases: Vec<Multihash>,
    server_certhashes: Vec<Multihash>,
) -> Result<(), noise::Error> {
    let client_id = identity::Keypair::generate_ed25519();
    let server_id = identity::Keypair::generate_ed25519();

    let (client, server) = futures_ringbuf::Endpoint::pair(100, 100);

    futures::executor::block_on(async move {
        let client_config = noise::Config::new(&client_id)?
            .with_webtransport_certhashes(valid_certhases.into_iter().collect());
        let server_config = noise::Config::new(&server_id)?
            .with_webtransport_certhashes(server_certhashes.into_iter().collect());

        let ((reported_client_id, mut _server_session), (reported_server_id, mut _client_session)) =
            futures::future::try_join(
                server_config.upgrade_inbound(server, ""),
                client_config.upgrade_outbound(client, ""),
            )
            .await?;

        assert_eq!(reported_client_id, client_id.public().to_peer_id());
        assert_eq!(reported_server_id, server_id.public().to_peer_id());

        Ok(())
    })
}

#[test]
fn webtransport_certhashes() {
    let certhash1 = Code::Sha2_256.digest(b"1");
    let certhash2 = Code::Sha2_256.digest(b"2");
    let certhash3 = Code::Sha2_256.digest(b"3");

    validate_webtransport_certhashes(vec![certhash1, certhash2], vec![certhash1, certhash2])
        .expect("same set of certhashes");

    validate_webtransport_certhashes(vec![certhash1], vec![certhash1, certhash2])
        .expect("subset of certhashes");

    // This is a valid case when certhashes are not self signed
    validate_webtransport_certhashes(vec![], vec![certhash1, certhash2])
        .expect("client without certhashes");

    // This is a valid case when certhashes are not self signed
    validate_webtransport_certhashes(vec![], vec![]).expect("client and server without certhashes");

    assert!(matches!(
        validate_webtransport_certhashes(vec![certhash1, certhash2], vec![])
            .expect_err("empty server certhashes"),
        noise::Error::UnknownWebTransportCerthashes(hashes)
            if hashes == [certhash1, certhash2].into_iter().collect()
    ));

    assert!(matches!(
        validate_webtransport_certhashes(vec![certhash1, certhash3], vec![certhash1, certhash2])
            .expect_err("different server certhashes"),
        noise::Error::UnknownWebTransportCerthashes(hashes)
            if hashes == [certhash3].into_iter().collect(),
    ));

    assert!(matches!(
        validate_webtransport_certhashes(vec![certhash1, certhash3], vec![certhash1])
            .expect_err("superset of certhashes"),
        noise::Error::UnknownWebTransportCerthashes(hashes)
            if hashes == [certhash3].into_iter().collect(),
    ));
}
