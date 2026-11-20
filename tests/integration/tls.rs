//! Postgres over TLS: `sslmode=require` and `verify-full` against a container with a certificate
//! of a private CA. Needs `podman` or `docker` (the test is skipped without), the image
//! `postgres:16` and a free local port.

use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use irisqo::{db::setup, models::DbOptions};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use sqlx::{Connection, PgConnection};

/// A container that is removed with the guard.
struct Container {
    engine: &'static str,
    name: String,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(self.engine)
            .args(["rm", "-f", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn engine() -> Option<&'static str> {
    ["podman", "docker"].into_iter().find(|e| {
        Command::new(e)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

#[tokio::test]
async fn postgres_over_tls_with_a_private_ca() {
    let Some(engine) = engine() else {
        eprintln!("skipped: neither podman nor docker");
        return;
    };

    // a CA and a server certificate for `localhost` only
    let ca_key = KeyPair::generate().expect("ca key");
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("ca params");
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "irisqo test CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = ca_params.self_signed(&ca_key).expect("ca");
    let issuer = Issuer::new(ca_params, ca_key);
    let server_key = KeyPair::generate().expect("server key");
    let server = CertificateParams::new(vec!["localhost".to_owned()])
        .expect("server params")
        .signed_by(&server_key, &issuer)
        .expect("server cert");

    let dir: PathBuf = std::env::temp_dir().join(format!("irisqo-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let ca_file = dir.join("ca.pem");
    std::fs::write(&ca_file, ca.pem()).expect("ca.pem");
    let ca_path = ca_file.to_str().expect("utf-8 path");

    // the certificate and the key travel as variables: no mounts, and the key gets the owner and
    // the mode Postgres insists on
    let name = format!("irisqo-tls-{}", std::process::id());
    let script = "d=/var/lib/postgresql; printf '%s' \"$SERVER_CRT\" > $d/server.crt; \
        printf '%s' \"$SERVER_KEY\" > $d/server.key; chown postgres:postgres $d/server.*; \
        chmod 600 $d/server.key; exec docker-entrypoint.sh postgres -c ssl=on \
        -c ssl_cert_file=$d/server.crt -c ssl_key_file=$d/server.key";
    let run = Command::new(engine)
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "-p",
            "127.0.0.1::5432",
        ])
        .args(["-e", "POSTGRES_PASSWORD=postgres"])
        .args(["-e", &format!("SERVER_CRT={}", server.pem())])
        .args(["-e", &format!("SERVER_KEY={}", server_key.serialize_pem())])
        .args(["postgres:16", "sh", "-c", script])
        .output()
        .expect("run");
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let _guard = Container {
        engine,
        name: name.clone(),
    };

    let port = Command::new(engine)
        .args(["port", &name, "5432"])
        .output()
        .expect("port");
    let port = String::from_utf8_lossy(&port.stdout)
        .lines()
        .next()
        .and_then(|l| l.rsplit_once(':'))
        .and_then(|(_, p)| p.trim().parse::<u16>().ok())
        .expect("the published port");

    let url = |host: &str, query: &str| {
        format!("postgres://postgres:postgres@{host}:{port}/postgres?{query}")
    };
    let connect = |url: String| async move {
        let mut conn = PgConnection::connect(&url).await?;
        let ssl: bool =
            sqlx::query_scalar("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
                .fetch_one(&mut conn)
                .await?;
        Ok::<_, sqlx::Error>(ssl)
    };

    // the server is up when it takes an encrypted connection
    let deadline = Instant::now() + Duration::from_secs(60);
    while connect(url("localhost", "sslmode=require")).await.is_err() {
        assert!(Instant::now() < deadline, "Postgres did not start");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // require encrypts without checking the certificate
    assert!(
        connect(url("localhost", "sslmode=require"))
            .await
            .expect("require")
    );
    // verify-full with the CA of the certificate
    assert!(
        connect(url(
            "localhost",
            &format!("sslmode=verify-full&sslrootcert={ca_path}")
        ))
        .await
        .expect("verify-full with the CA")
    );
    // ... and through the pool of the service
    let pool = setup::connect(
        &DbOptions::new(url(
            "localhost",
            &format!("sslmode=verify-full&sslrootcert={ca_path}"),
        )),
        "tls-test",
    )
    .await;
    pool.close().await;
    // the CA is private: the system store does not know it
    assert!(
        connect(url("localhost", "sslmode=verify-full"))
            .await
            .is_err()
    );
    // the certificate is for `localhost`, not for the address
    assert!(
        connect(url(
            "127.0.0.1",
            &format!("sslmode=verify-full&sslrootcert={ca_path}")
        ))
        .await
        .is_err()
    );
    // verify-ca does not check the name
    assert!(
        connect(url(
            "127.0.0.1",
            &format!("sslmode=verify-ca&sslrootcert={ca_path}")
        ))
        .await
        .expect("verify-ca")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
