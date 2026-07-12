//! The constant protocol-card fallback for any UNMATCHED path — identical for every path (no existence
//! signal), content-negotiated, and non-GET unmatched staying the uniform JSON 404.

use topos_types::requests::WireProtocolCard;

use super::*;

/// The markdown card is byte-identical for three DIFFERENT unmatched paths (no path echo — an unmatched
/// path is never an existence oracle), and it carries the human hand-off + the one agent command.
#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn the_card_fallback_is_identical_for_every_path(pool: PgPool) {
    let ctx = enroll_setup(pool, "card-identical").await;
    let paths = ["/acme", "/acme/channels/deploy", "/totally/made/up"];
    let mut bodies = Vec::new();
    for path in paths {
        let (status, headers, bytes) = send(ctx.app(), get(path, &[("accept", "*/*")])).await;
        assert_eq!(status, StatusCode::OK, "the card fallback serves {path}");
        assert!(
            headers
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/plain"),
            "the markdown card for {path}"
        );
        assert_eq!(headers.get("cache-control").unwrap(), "no-store");
        assert_eq!(headers.get("vary").unwrap(), "accept");
        assert_eq!(headers.get("x-robots-tag").unwrap(), "noindex");
        bodies.push(String::from_utf8(bytes.to_vec()).unwrap());
    }
    // Byte-identical across the three paths — the card echoes NO path.
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(bodies[1], bodies[2]);
    let doc = &bodies[0];
    assert!(doc.contains("A Topos resource address"));
    assert!(doc.contains("paste this URL to your agent"));
    assert!(doc.contains("topos follow '<the URL you just fetched>' --json"));
    assert!(doc.contains("releases/latest/download/install.sh"));
    // No path leaked into the body.
    assert!(!doc.contains("/totally/made/up"));
}

/// The JSON face of the card carries the API base URL a client re-roots onto, and the constant discriminant.
#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn the_card_json_face_carries_the_api_base(pool: PgPool) {
    let ctx = enroll_setup(pool, "card-json").await;
    let (status, headers, bytes) = send(
        ctx.app(),
        get("/some/unmatched/path", &[("accept", "application/json")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let card: WireProtocolCard = serde_json::from_slice(&bytes).expect("a WireProtocolCard");
    assert_eq!(card.schema_version, 1);
    assert_eq!(card.card, "topos-protocol-card");
    assert_eq!(card.api_base_url, ENROLL_BASE_URL);
}

/// A NON-GET unmatched path is the uniform JSON 404 (there is nothing to teach a mutation that routes
/// nowhere), never the card.
#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_non_get_unmatched_path_is_the_uniform_404(pool: PgPool) {
    let ctx = enroll_setup(pool, "card-non-get").await;
    let (status, headers, bytes) = send(
        ctx.app(),
        post_nosig("/some/unmatched/path", serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let env = envelope(&bytes);
    assert!(!env.ok, "the uniform 404 envelope: {env:?}");
}
