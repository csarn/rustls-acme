use crate::acceptor::AcmeAcceptor;
use crate::acme::{Account, AcmeError, Auth, Directory, Identifier, Order};
use crate::{AcmeConfig, Incoming, ResolvesServerCertAcme};
use async_io::Timer;
use chrono::{DateTime, TimeZone, Utc};
use futures::future::try_join_all;
use futures::prelude::*;
use futures::ready;
use pin_project::pin_project;
use rcgen::{CertificateParams, DistinguishedName, RcgenError, PKCS_ECDSA_P256_SHA256};
use rustls::sign::{any_ecdsa_type, CertifiedKey};
use rustls::Certificate as RustlsCertificate;
use rustls::PrivateKey;
use std::convert::Infallible;
use std::fmt::Debug;
use std::future::Future;
use std::ops::Div;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use thiserror::Error;
use x509_parser::parse_x509_certificate;

#[pin_project()]
pub struct AcmeState<EC: Debug = Infallible, EA: Debug = EC> {
    config: Arc<AcmeConfig<EC, EA>>,
    resolver: Arc<ResolvesServerCertAcme>,
    account_key: Option<Vec<u8>>,

    early_action: Option<Pin<Box<dyn Future<Output = Event<EC, EA>> + Send>>>,
    load_cert: Option<Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, EC>> + Send>>>,
    load_account: Option<Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, EA>> + Send>>>,
    order: Option<Pin<Box<dyn Future<Output = Result<Vec<u8>, OrderError>> + Send>>>,
    valid_until: chrono::DateTime<Utc>,
    backoff_cnt: usize,
    wait: Option<Timer>,
}

pub type Event<EC, EA> = Result<EventOk, EventError<EC, EA>>;

#[derive(Debug)]
pub enum EventOk {
    DeployedCachedCert,
    DeployedNewCert,
    CertCacheStore,
    AccountCacheStore,
}

#[derive(Error, Debug)]
pub enum EventError<EC: Debug, EA: Debug> {
    #[error("cert cache load: {0}")]
    CertCacheLoad(EC),
    #[error("account cache load: {0}")]
    AccountCacheLoad(EA),
    #[error("cert cache store: {0}")]
    CertCacheStore(EC),
    #[error("account cache store: {0}")]
    AccountCacheStore(EA),
    #[error("cached cert parse: {0}")]
    CachedCertParse(CertParseError),
    #[error("order: {0}")]
    Order(OrderError),
    #[error("new cert parse: {0}")]
    NewCertParse(CertParseError),
}

#[derive(Error, Debug)]
pub enum OrderError {
    #[error("acme error: {0}")]
    Acme(#[from] AcmeError),
    #[error("certificate generation error: {0}")]
    Rcgen(#[from] RcgenError),
    #[error("bad order object: {0:?}")]
    BadOrder(Order),
    #[error("bad auth object: {0:?}")]
    BadAuth(Auth),
    #[error("authorization for {0} failed too many times")]
    TooManyAttemptsAuth(String),
}

#[derive(Error, Debug)]
pub enum CertParseError {
    #[error("X509 parsing error: {0}")]
    X509(#[from] x509_parser::nom::Err<x509_parser::error::X509Error>),
    #[error("expected 2 or more pem, got: {0}")]
    Pem(#[from] pem::PemError),
    #[error("expected 2 or more pem, got: {0}")]
    TooFewPem(usize),
    #[error("unsupported private key type")]
    InvalidPrivateKey,
}

impl<EC: 'static + Debug, EA: 'static + Debug> AcmeState<EC, EA> {
    pub fn incoming<
        TCP: AsyncRead + AsyncWrite + Unpin,
        ETCP,
        ITCP: Stream<Item = Result<TCP, ETCP>> + Unpin,
    >(
        self,
        tcp_incoming: ITCP,
    ) -> Incoming<TCP, ETCP, ITCP, EC, EA> {
        let acceptor = self.acceptor();
        Incoming::new(tcp_incoming, self, acceptor)
    }
    pub fn acceptor(&self) -> AcmeAcceptor {
        AcmeAcceptor::new(self.resolver())
    }
    #[cfg(feature = "axum")]
    pub fn axum_acceptor(
        &self,
        rustls_config: Arc<rustls::ServerConfig>,
    ) -> crate::axum::AxumAcceptor {
        crate::axum::AxumAcceptor::new(self.acceptor(), rustls_config)
    }
    pub fn resolver(&self) -> Arc<ResolvesServerCertAcme> {
        self.resolver.clone()
    }
    pub fn new(config: AcmeConfig<EC, EA>) -> Self {
        let config = Arc::new(config);
        Self {
            config: config.clone(),
            resolver: ResolvesServerCertAcme::new(),
            account_key: None,
            early_action: None,
            load_cert: Some(Box::pin({
                let config = config.clone();
                async move {
                    config
                        .cache
                        .load_cert(&config.domains, &config.directory_url)
                        .await
                }
            })),
            load_account: Some(Box::pin({
                let config = config.clone();
                async move {
                    config
                        .cache
                        .load_account(&config.contact, &config.directory_url)
                        .await
                }
            })),
            order: None,
            valid_until: Utc.timestamp(0, 0),
            backoff_cnt: 0,
            wait: None,
        }
    }
    fn parse_cert(pem: &[u8]) -> Result<(CertifiedKey, DateTime<Utc>), CertParseError> {
        let mut pems = pem::parse_many(&pem)?;
        if pems.len() < 2 {
            return Err(CertParseError::TooFewPem(pems.len()));
        }
        let pk = match any_ecdsa_type(&PrivateKey(pems.remove(0).contents)) {
            Ok(pk) => pk,
            Err(_) => return Err(CertParseError::InvalidPrivateKey),
        };
        let cert_chain: Vec<RustlsCertificate> = pems
            .into_iter()
            .map(|p| RustlsCertificate(p.contents))
            .collect();
        let valid_until = match parse_x509_certificate(cert_chain[0].0.as_slice()) {
            Ok((_, cert)) => Utc.timestamp(cert.validity().not_after.timestamp(), 0),
            Err(err) => return Err(CertParseError::X509(err)),
        };
        let cert = CertifiedKey::new(cert_chain, pk);
        Ok((cert, valid_until))
    }
    fn process_cert(&mut self, pem: Vec<u8>, cached: bool) -> Event<EC, EA> {
        let (cert, valid_until) = match (Self::parse_cert(&pem), cached) {
            (Ok(r), _) => r,
            (Err(err), cached) => {
                return match cached {
                    true => Err(EventError::CachedCertParse(err)),
                    false => Err(EventError::NewCertParse(err)),
                }
            }
        };
        self.valid_until = valid_until;
        self.resolver.set_cert(Arc::new(cert));
        let wait_duration = (self.valid_until - Utc::now())
            .max(chrono::Duration::zero())
            .div(2)
            .to_std()
            .unwrap_or_default();
        self.wait = Some(Timer::after(wait_duration));
        if cached {
            return Ok(EventOk::DeployedCachedCert);
        }
        let config = self.config.clone();
        self.early_action = Some(Box::pin(async move {
            match config
                .cache
                .store_cert(&config.domains, &config.directory_url, &pem)
                .await
            {
                Ok(()) => Ok(EventOk::CertCacheStore),
                Err(err) => Err(EventError::CertCacheStore(err)),
            }
        }));
        Event::Ok(EventOk::DeployedNewCert)
    }
    async fn order(
        config: Arc<AcmeConfig<EC, EA>>,
        resolver: Arc<ResolvesServerCertAcme>,
        key_pair: Vec<u8>,
    ) -> Result<Vec<u8>, OrderError> {
        let directory = Directory::discover(&config.directory_url).await?;
        let account = Account::create_with_keypair(directory, &config.contact, &key_pair).await?;

        let mut params = CertificateParams::new(config.domains.clone());
        params.distinguished_name = DistinguishedName::new();
        params.alg = &PKCS_ECDSA_P256_SHA256;
        let cert = rcgen::Certificate::from_params(params)?;

        let mut order = account.new_order(config.domains.clone()).await?;
        loop {
            order = match order {
                Order::Pending {
                    authorizations,
                    finalize,
                } => {
                    let auth_futures = authorizations
                        .iter()
                        .map(|url| Self::authorize(&resolver, &account, url));
                    try_join_all(auth_futures).await?;
                    log::info!("completed all authorizations");
                    Order::Ready { finalize }
                }
                Order::Ready { finalize } => {
                    log::info!("sending csr");
                    let csr = cert.serialize_request_der()?;
                    account.finalize(finalize, csr).await?
                }
                Order::Valid { certificate } => {
                    log::info!("download certificate");
                    let pem = [
                        &cert.serialize_private_key_pem(),
                        "\n",
                        &account.certificate(certificate).await?,
                    ]
                    .concat();
                    return Ok(pem.into_bytes());
                }
                Order::Invalid => return Err(OrderError::BadOrder(order)),
            }
        }
    }
    async fn authorize(
        resolver: &ResolvesServerCertAcme,
        account: &Account,
        url: &String,
    ) -> Result<(), OrderError> {
        let (domain, challenge_url) = match account.auth(url).await? {
            Auth::Pending {
                identifier,
                challenges,
            } => {
                let Identifier::Dns(domain) = identifier;
                log::info!("trigger challenge for {}", &domain);
                let (challenge, auth_key) = account.tls_alpn_01(&challenges, domain.clone())?;
                resolver.set_auth_key(domain.clone(), Arc::new(auth_key));
                account.challenge(&challenge.url).await?;
                (domain, challenge.url.clone())
            }
            Auth::Valid => return Ok(()),
            auth => return Err(OrderError::BadAuth(auth)),
        };
        for i in 0u64..5 {
            Timer::after(Duration::from_secs(1u64 << i)).await;
            match account.auth(url).await? {
                Auth::Pending { .. } => {
                    log::info!("authorization for {} still pending", &domain);
                    account.challenge(&challenge_url).await?
                }
                Auth::Valid => return Ok(()),
                auth => return Err(OrderError::BadAuth(auth)),
            }
        }
        Err(OrderError::TooManyAttemptsAuth(domain))
    }
    fn poll_next_infinite(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Event<EC, EA>> {
        loop {
            // queued early action
            if let Some(early_action) = &mut self.early_action {
                let result = ready!(early_action.poll_unpin(cx));
                self.early_action.take();
                return Poll::Ready(result);
            }

            // sleep
            if let Some(timer) = &mut self.wait {
                ready!(timer.poll_unpin(cx));
                self.wait.take();
            }

            // load from cert cache
            if let Some(load_cert) = &mut self.load_cert {
                let result = ready!(load_cert.poll_unpin(cx));
                self.load_cert.take();
                match result {
                    Ok(Some(pem)) => {
                        return Poll::Ready(Self::process_cert(self.get_mut(), pem, true));
                    }
                    Ok(None) => {}
                    Err(err) => return Poll::Ready(Err(EventError::CertCacheLoad(err))),
                }
            }

            // load from account cache
            if let Some(load_account) = &mut self.load_account {
                let result = ready!(load_account.poll_unpin(cx));
                self.load_account.take();
                match result {
                    Ok(Some(key_pair)) => self.account_key = Some(key_pair),
                    Ok(None) => {}
                    Err(err) => return Poll::Ready(Err(EventError::AccountCacheLoad(err))),
                }
            }

            // execute order
            if let Some(order) = &mut self.order {
                let result = ready!(order.poll_unpin(cx));
                self.order.take();
                match result {
                    Ok(pem) => {
                        self.backoff_cnt = 0;
                        return Poll::Ready(Self::process_cert(self.get_mut(), pem, false));
                    }
                    Err(err) => {
                        // TODO: replace key on some errors or high backoff_cnt?
                        self.wait = Some(Timer::after(Duration::from_secs(1 << self.backoff_cnt)));
                        self.backoff_cnt = (self.backoff_cnt + 1).min(16);
                        return Poll::Ready(Err(EventError::Order(err)));
                    }
                }
            }

            // schedule order
            let account_key = match &self.account_key {
                None => {
                    let account_key = Account::generate_key_pair();
                    self.account_key = Some(account_key.clone());
                    let config = self.config.clone();
                    let account_key_clone = account_key.clone();
                    self.early_action = Some(Box::pin(async move {
                        match config
                            .cache
                            .store_account(
                                &config.contact,
                                &config.directory_url,
                                &account_key_clone,
                            )
                            .await
                        {
                            Ok(()) => Ok(EventOk::AccountCacheStore),
                            Err(err) => Err(EventError::AccountCacheStore(err)),
                        }
                    }));
                    account_key
                }
                Some(account_key) => account_key.clone(),
            };
            let config = self.config.clone();
            let resolver = self.resolver.clone();
            self.order = Some(Box::pin({
                Self::order(config.clone(), resolver.clone(), account_key)
            }));
        }
    }
}

impl<EC: 'static + Debug, EA: 'static + Debug> Stream for AcmeState<EC, EA> {
    type Item = Event<EC, EA>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(Some(ready!(self.poll_next_infinite(cx))))
    }
}
