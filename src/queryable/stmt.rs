// Copyright (c) 2017 Anatoly Ikorsky
//
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. All files in the project carrying such notice may not be copied,
// modified, or distributed except according to those terms.

use mysql_common::{
    constants::MAX_PAYLOAD_LEN,
    packets::{
        column_from_payload, parse_stmt_packet, ComStmtClose, ComStmtExecuteRequestBuilder,
        ComStmtSendLongData, StmtPacket,
    },
};

use std::{borrow::Cow, sync::Arc};

use crate::{
    conn::named_params::parse_named_params,
    connection_like::ConnectionLike,
    consts::{CapabilityFlags, Command},
    error::*,
    queryable::{
        query_result::{read_result_set, QueryResult},
        BinaryProtocol,
    },
    Column, Params, Value,
};

pub trait StatementLike: Send + Sync {
    /// Returns raw statement query coupled with its nemed parameters.
    fn info(&self) -> Result<(Option<Vec<String>>, Cow<str>)>;
}

impl StatementLike for str {
    fn info(&self) -> Result<(Option<Vec<String>>, Cow<str>)> {
        parse_named_params(self).map_err(From::from)
    }
}

impl StatementLike for Statement {
    fn info(&self) -> Result<(Option<Vec<String>>, Cow<str>)> {
        Ok((
            self.named_params.clone(),
            self.inner.raw_query.as_ref().into(),
        ))
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct StmtInner {
    pub(crate) raw_query: Arc<str>,
    columns: Option<Box<[Column]>>,
    params: Option<Box<[Column]>>,
    stmt_packet: StmtPacket,
    connection_id: u32,
}

impl StmtInner {
    pub(crate) fn from_payload(
        pld: &[u8],
        connection_id: u32,
        raw_query: Arc<str>,
    ) -> std::io::Result<Self> {
        let stmt_packet = parse_stmt_packet(pld)?;

        Ok(Self {
            raw_query,
            columns: None,
            params: None,
            stmt_packet,
            connection_id,
        })
    }

    pub(crate) fn with_params(mut self, params: Vec<Column>) -> Self {
        self.params = if params.is_empty() {
            None
        } else {
            Some(params.into_boxed_slice())
        };
        self
    }

    pub(crate) fn with_columns(mut self, columns: Vec<Column>) -> Self {
        self.columns = if columns.is_empty() {
            None
        } else {
            Some(columns.into_boxed_slice())
        };
        self
    }

    pub(crate) fn columns(&self) -> &[Column] {
        self.columns.as_ref().map(AsRef::as_ref).unwrap_or(&[])
    }

    pub(crate) fn params(&self) -> &[Column] {
        self.params.as_ref().map(AsRef::as_ref).unwrap_or(&[])
    }

    pub(crate) fn id(&self) -> u32 {
        self.stmt_packet.statement_id()
    }

    pub(crate) const fn connection_id(&self) -> u32 {
        self.connection_id
    }

    pub(crate) fn num_params(&self) -> u16 {
        self.stmt_packet.num_params()
    }

    pub(crate) fn num_columns(&self) -> u16 {
        self.stmt_packet.num_columns()
    }
}

/// Prepared statement.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Statement {
    pub(crate) inner: Arc<StmtInner>,
    pub(crate) named_params: Option<Vec<String>>,
}

impl Statement {
    pub(crate) fn new(inner: Arc<StmtInner>, named_params: Option<Vec<String>>) -> Self {
        Self {
            inner,
            named_params,
        }
    }

    pub fn columns(&self) -> &[Column] {
        self.inner.columns()
    }

    pub fn params(&self) -> &[Column] {
        self.inner.params()
    }

    pub fn id(&self) -> u32 {
        self.inner.id()
    }

    pub fn connection_id(&self) -> u32 {
        self.inner.connection_id()
    }

    pub fn num_params(&self) -> u16 {
        self.inner.num_params()
    }

    pub fn num_columns(&self) -> u16 {
        self.inner.num_columns()
    }
}

impl crate::Conn {
    /// Low-level helpers, that reads the given number of column packets from server.
    ///
    /// Requires `num > 0`.
    async fn read_column_defs<U>(&mut self, num: U) -> Result<Vec<Column>>
    where
        U: Into<usize>,
    {
        let num = num.into();
        debug_assert!(num > 0);
        let packets = self.read_packets(num).await?;
        let defs = packets
            .into_iter()
            .map(column_from_payload)
            .collect::<std::result::Result<Vec<Column>, _>>()
            .map_err(Error::from)?;

        if !self
            .conn_ref()
            .capabilities()
            .contains(CapabilityFlags::CLIENT_DEPRECATE_EOF)
        {
            self.read_packet().await?;
        }

        Ok(defs)
    }

    /// Helper, that retrieves `Statement` from `StatementLike`.
    pub(crate) async fn get_statement<U>(&mut self, stmt_like: &U) -> Result<Statement>
    where
        U: StatementLike + ?Sized,
    {
        let (named_params, raw_query) = stmt_like.info()?;
        let stmt_inner = if let Some(stmt_inner) = self.get_cached_stmt(raw_query.as_ref()) {
            stmt_inner
        } else {
            self.prepare_statement(raw_query).await?
        };
        Ok(Statement::new(stmt_inner, named_params))
    }

    /// Low-level helper, that prepares the given statement.
    ///
    /// `raw_query` is a query with `?` placeholders (if any).
    async fn prepare_statement(&mut self, raw_query: Cow<'_, str>) -> Result<Arc<StmtInner>> {
        let raw_query: Arc<str> = raw_query.into_owned().into_boxed_str().into();

        self.write_command_data(Command::COM_STMT_PREPARE, raw_query.as_bytes())
            .await?;

        let packet = self.read_packet().await?;
        let mut inner_stmt = StmtInner::from_payload(&*packet, self.conn_ref().id(), raw_query)?;

        if inner_stmt.num_params() > 0 {
            let params = self.read_column_defs(inner_stmt.num_params()).await?;
            inner_stmt = inner_stmt.with_params(params);
        }

        if inner_stmt.num_columns() > 0 {
            let columns = self.read_column_defs(inner_stmt.num_columns()).await?;
            inner_stmt = inner_stmt.with_columns(columns);
        }

        let inner_stmt = Arc::new(inner_stmt);

        if let Some(old_stmt) = self.conn_mut().cache_stmt(&inner_stmt) {
            self.close_statement(old_stmt.id()).await?;
        }

        Ok(inner_stmt)
    }

    /// Helper, that executes the given statement with the given params.
    pub(crate) async fn execute_statement<P>(
        &mut self,
        statement: &Statement,
        params: P,
    ) -> Result<QueryResult<'_, Self, BinaryProtocol>>
    where
        P: Into<Params>,
    {
        let mut params = params.into();
        loop {
            match params {
                Params::Positional(params) => {
                    if statement.num_params() as usize != params.len() {
                        Err(DriverError::StmtParamsMismatch {
                            required: statement.num_params(),
                            supplied: params.len() as u16,
                        })?
                    }

                    let params = params.into_iter().collect::<Vec<_>>();

                    let (body, as_long_data) =
                        ComStmtExecuteRequestBuilder::new(statement.id()).build(&*params);

                    if as_long_data {
                        self.send_long_data(statement.id(), params.iter()).await?;
                    }

                    self.write_command_raw(body).await?;
                    break read_result_set(self).await;
                }
                Params::Named(_) => {
                    if statement.named_params.is_none() {
                        let error = DriverError::NamedParamsForPositionalQuery.into();
                        return Err(error);
                    }

                    params = match params.into_positional(statement.named_params.as_ref().unwrap())
                    {
                        Ok(positional_params) => positional_params,
                        Err(error) => return Err(error.into()),
                    };

                    continue;
                }
                Params::Empty => {
                    if statement.num_params() > 0 {
                        let error = DriverError::StmtParamsMismatch {
                            required: statement.num_params(),
                            supplied: 0,
                        }
                        .into();
                        return Err(error);
                    }

                    let (body, _) = ComStmtExecuteRequestBuilder::new(statement.id()).build(&[]);
                    self.write_command_raw(body).await?;
                    break read_result_set(self).await;
                }
            }
        }
    }

    /// Helper, that sends all `Value::Bytes` in the given list of paramenters as long data.
    async fn send_long_data<'a, I>(&mut self, statement_id: u32, params: I) -> Result<()>
    where
        I: Iterator<Item = &'a Value>,
    {
        for (i, value) in params.enumerate() {
            if let Value::Bytes(bytes) = value {
                let chunks = bytes.chunks(MAX_PAYLOAD_LEN - 6);
                let chunks = chunks.chain(if bytes.is_empty() {
                    Some(&[][..])
                } else {
                    None
                });
                for chunk in chunks {
                    let com = ComStmtSendLongData::new(statement_id, i, chunk);
                    self.write_command_raw(com.into()).await?;
                }
            }
        }

        Ok(())
    }

    /// Helper, that closes statement with the given id.
    pub(crate) async fn close_statement(&mut self, id: u32) -> Result<()> {
        self.write_command_raw(ComStmtClose::new(id).into()).await
    }
}
