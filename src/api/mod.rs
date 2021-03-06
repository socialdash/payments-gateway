mod controllers;
mod error;
mod requests;
pub mod responses;
mod utils;

use std::net::SocketAddr;
use std::sync::Arc;

use base64;
use diesel::pg::PgConnection;
use diesel::r2d2::ConnectionManager;
use failure::{Compat, Fail};
use futures::prelude::*;
use futures_cpupool::CpuPool;
use hyper;
use hyper::Server;
use hyper::{service::Service, Body, Request, Response};
use r2d2::Pool;

use self::controllers::*;
use self::error::*;
use super::config::Config;
use super::utils::{log_and_capture_error, log_error, log_warn};
use client::{HttpClientImpl, StoriqaClient, StoriqaClientImpl, TransactionsClient, TransactionsClientImpl};
use models::*;
use r2d2;
use rabbit::TransactionPublisher;
use repos::{
    AccountsRepoImpl, DbExecutorImpl, DeviceTokensRepoImpl, DevicesRepoImpl, TemplatesRepoImpl, TransactionFiatRepoImpl, UsersRepoImpl,
};
use services::{AccountsServiceImpl, AuthServiceImpl, EmailSenderServiceImpl, TransactionsServiceImpl, UsersServiceImpl};
use utils::read_body;

#[derive(Clone)]
pub struct ApiService {
    storiqa_client: Arc<dyn StoriqaClient>,
    storiqa_jwt_public_key: Vec<u8>,
    server_address: SocketAddr,
    config: Arc<Config>,
    db_pool: Pool<ConnectionManager<PgConnection>>,
    cpu_pool: CpuPool,
    transactions_client: Arc<dyn TransactionsClient>,
    publisher: Arc<dyn TransactionPublisher>,
}

impl ApiService {
    fn from_config(config: Config, publisher: Arc<dyn TransactionPublisher>) -> Result<Self, Error> {
        let client = HttpClientImpl::new(&config);
        let storiqa_client = StoriqaClientImpl::new(&config, client.clone());
        let storiqa_jwt_public_key_base64 = config.auth.storiqa_jwt_public_key_base64.clone();
        let storiqa_jwt_public_key = base64::decode(&config.auth.storiqa_jwt_public_key_base64).map_err(ectx!(try
            ErrorContext::Config,
            ErrorKind::Internal =>
            storiqa_jwt_public_key_base64
        ))?;
        let host = config.server.host.clone();
        let port = config.server.port.clone();
        let server_address = format!("{}:{}", host, port).parse::<SocketAddr>().map_err(ectx!(try
            ErrorContext::Config,
            ErrorKind::Internal =>
            host,
            port
        ))?;
        let database_url = config.database.url.clone();
        let manager = ConnectionManager::<PgConnection>::new(database_url.clone());
        let db_pool = r2d2::Pool::builder().build(manager).map_err(ectx!(try
            ErrorContext::Config,
            ErrorKind::Internal =>
            database_url
        ))?;
        let cpu_pool = CpuPool::new(config.cpu_pool.size);
        let transactions_client = TransactionsClientImpl::new(&config, client);
        Ok(ApiService {
            config: Arc::new(config),
            storiqa_client: Arc::new(storiqa_client),
            storiqa_jwt_public_key,
            server_address,
            db_pool,
            cpu_pool,
            transactions_client: Arc::new(transactions_client),
            publisher,
        })
    }
}

impl Service for ApiService {
    type ReqBody = Body;
    type ResBody = Body;
    type Error = Compat<Error>;
    type Future = Box<Future<Item = Response<Body>, Error = Self::Error> + Send>;

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let (parts, http_body) = req.into_parts();
        let storiqa_client = self.storiqa_client.clone();
        let storiqa_jwt_public_key = self.storiqa_jwt_public_key.clone();
        let storiqa_jwt_valid_secs = self.config.auth.storiqa_jwt_valid_secs.clone();
        let device_add_token_expiration = self.config.auth.device_add_token_valid_secs.clone();
        let email_sending_timeout = self.config.auth.email_sending_timeout_secs.clone();
        let device_confirm_url = self.config.notifications.device_confirm_url.clone();
        let db_pool = self.db_pool.clone();
        let cpu_pool = self.cpu_pool.clone();
        let db_executor = DbExecutorImpl::new(db_pool.clone(), cpu_pool.clone());
        let transactions_client = self.transactions_client.clone();
        let publisher = self.publisher.clone();
        let config = self.config.clone();

        Box::new(
            read_body(http_body)
                .map_err(ectx!(ErrorSource::Hyper, ErrorKind::Internal))
                .and_then(move |body| {
                    let router = router! {
                        POST /v1/sessions => post_sessions,
                        POST /v1/sessions/oauth => post_sessions_oauth,
                        POST /v1/sessions/refresh => post_sessions_refresh,
                        POST /v1/sessions/revoke => post_sessions_revoke,
                        POST /v1/users => post_users,
                        PUT /v1/users => put_users,
                        POST /v1/users/add_device => post_users_add_device,
                        POST /v1/users/confirm_add_device => post_users_confirm_add_device,
                        POST /v1/users/confirm_email => post_users_confirm_email,
                        POST /v1/users/resend_confirm_email => post_users_resend_confirm_email,
                        POST /v1/users/reset_password => post_users_reset_password,
                        POST /v1/users/change_password => post_users_change_password,
                        POST /v1/users/confirm_reset_password => post_users_confirm_reset_password,
                        GET /v1/users/me => get_users_me,
                        POST /v1/users/{user_id: UserId}/accounts => post_accounts,
                        GET /v1/users/{user_id: UserId}/accounts => get_users_accounts,
                        GET /v1/accounts/{account_id: AccountId} => get_accounts,
                        PUT /v1/accounts/{account_id: AccountId} => put_accounts,
                        DELETE /v1/accounts/{account_id: AccountId} => delete_accounts,
                        GET /v1/accounts/{account_id: AccountId}/transactions => get_accounts_transactions,
                        GET /v1/users/{user_id: UserId}/transactions => get_users_transactions,
                        GET /v1/transactions/{tx_id: TransactionId} => get_transaction,
                        POST /v1/transactions => post_transactions,
                        POST /v1/rate => post_rate,
                        POST /v1/rate/refresh => post_rate_refresh,
                        POST /v1/fees => post_fees,
                        GET /wallet/register_device/{token: DeviceConfirmToken} => get_register_device,
                        GET /wallet/verify_email/{token: EmailConfirmToken} => get_users_confirm_email,
                        GET /wallet/reset_password/{token: PasswordResetToken} => get_confirm_reset_password,
                        _ => not_found,
                    };

                    let auth_service = Arc::new(AuthServiceImpl::new(
                        storiqa_jwt_public_key,
                        storiqa_jwt_valid_secs,
                        Arc::new(DevicesRepoImpl),
                        Arc::new(UsersRepoImpl),
                        db_executor.clone(),
                    ));
                    let email_service = Arc::new(EmailSenderServiceImpl::new(
                        Arc::new(TemplatesRepoImpl),
                        db_executor.clone(),
                        publisher,
                        device_confirm_url,
                    ));
                    let users_service = Arc::new(UsersServiceImpl::new(
                        storiqa_client,
                        Arc::new(UsersRepoImpl),
                        Arc::new(DevicesRepoImpl),
                        Arc::new(DeviceTokensRepoImpl),
                        db_executor.clone(),
                        email_service,
                        device_add_token_expiration,
                        email_sending_timeout,
                    ));
                    let accounts_service = Arc::new(AccountsServiceImpl::new(
                        Arc::new(AccountsRepoImpl),
                        db_executor.clone(),
                        transactions_client.clone(),
                    ));
                    let transactions_service = Arc::new(TransactionsServiceImpl::new(
                        Arc::new(AccountsRepoImpl),
                        Arc::new(UsersRepoImpl),
                        Arc::new(TransactionFiatRepoImpl),
                        db_executor.clone(),
                        transactions_client,
                    ));

                    let ctx = Context {
                        body,
                        method: parts.method.clone(),
                        uri: parts.uri.clone(),
                        headers: parts.headers,
                        users_service,
                        accounts_service,
                        transactions_service,
                        auth_service,
                        config,
                    };

                    debug!("Received request {}", ctx);

                    router(ctx, parts.method.into(), parts.uri.path())
                })
                .and_then(|resp| {
                    let (parts, body) = resp.into_parts();
                    read_body(body)
                        .map_err(ectx!(ErrorSource::Hyper, ErrorKind::Internal))
                        .map(|body| (parts, body))
                })
                .map(|(parts, body)| {
                    debug!(
                        "Sent response with status {}, headers: {:#?}, body: {:?}",
                        parts.status.as_u16(),
                        parts.headers,
                        String::from_utf8(body.clone()).ok()
                    );
                    Response::from_parts(parts, body.into())
                })
                .or_else(|e| match e.kind() {
                    ErrorKind::BadRequest => {
                        log_error(&e);
                        Ok(Response::builder()
                            .status(400)
                            .header("Content-Type", "application/json")
                            .body(Body::from(r#"{"description": "Bad request"}"#))
                            .unwrap())
                    }
                    ErrorKind::Unauthorized => {
                        log_warn(&e);
                        Ok(Response::builder()
                            .status(401)
                            .header("Content-Type", "application/json")
                            .body(Body::from(r#"{"description": "Unauthorized"}"#))
                            .unwrap())
                    }
                    ErrorKind::NotFound => {
                        log_warn(&e);
                        Ok(Response::builder()
                            .status(404)
                            .header("Content-Type", "application/json")
                            .body(Body::from(r#"{"description": "Not found"}"#))
                            .unwrap())
                    }
                    ErrorKind::UnprocessableEntity(errors) => {
                        log_warn(&e);
                        Ok(Response::builder()
                            .status(422)
                            .header("Content-Type", "application/json")
                            .body(Body::from(errors))
                            .unwrap())
                    }
                    ErrorKind::Internal => {
                        log_and_capture_error(e);
                        Ok(Response::builder()
                            .status(500)
                            .header("Content-Type", "application/json")
                            .body(Body::from(r#"{"description": "Internal server error"}"#))
                            .unwrap())
                    }
                }),
        )
    }
}

pub fn server(config: Config, publisher: Arc<dyn TransactionPublisher>) -> Box<Future<Item = (), Error = ()> + Send> {
    let fut = ApiService::from_config(config, publisher)
        .into_future()
        .and_then(move |api| {
            let api_clone = api.clone();
            let new_service = move || {
                let res: Result<_, hyper::Error> = Ok(api_clone.clone());
                res
            };
            let addr = api.server_address.clone();
            let server = Server::bind(&api.server_address)
                .serve(new_service)
                .map_err(ectx!(ErrorSource::Hyper, ErrorKind::Internal => addr));
            info!("Listening on http://{}", addr);
            server
        })
        .map_err(|e: Error| log_error(&e));

    Box::new(fut)
}
