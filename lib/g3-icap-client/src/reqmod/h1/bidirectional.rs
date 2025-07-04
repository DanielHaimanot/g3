/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2023-2025 ByteDance and/or its affiliates.
 */

use std::sync::Arc;

use anyhow::anyhow;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite};

use g3_http::{H1BodyToChunkedTransfer, HttpBodyDecodeReader, HttpBodyReader};
use g3_io_ext::{IdleCheck, LimitedBufReadExt, StreamCopy, StreamCopyConfig, StreamCopyError};

use super::{
    H1ReqmodAdaptationError, HttpAdaptedRequest, HttpRequestForAdaptation,
    HttpRequestUpstreamWriter, ReqmodAdaptationEndState, ReqmodAdaptationRunState,
};
use crate::reqmod::response::ReqmodResponse;
use crate::{IcapClientReader, IcapClientWriter, IcapServiceClient};

pub(super) struct BidirectionalRecvIcapResponse<'a, I: IdleCheck> {
    pub(super) icap_client: &'a Arc<IcapServiceClient>,
    pub(super) icap_reader: &'a mut IcapClientReader,
    pub(super) idle_checker: &'a I,
}

impl<I: IdleCheck> BidirectionalRecvIcapResponse<'_, I> {
    pub(super) async fn transfer_and_recv<CR>(
        self,
        mut body_transfer: &mut H1BodyToChunkedTransfer<'_, CR, IcapClientWriter>,
    ) -> Result<ReqmodResponse, H1ReqmodAdaptationError>
    where
        CR: AsyncBufRead + Unpin,
    {
        let mut idle_interval = self.idle_checker.interval_timer();
        let mut idle_count = 0;

        loop {
            tokio::select! {
                biased;

                r = &mut body_transfer => {
                    return match r {
                        Ok(_) => self.recv_icap_response().await,
                        Err(StreamCopyError::ReadFailed(e)) => Err(H1ReqmodAdaptationError::HttpClientReadFailed(e)),
                        Err(StreamCopyError::WriteFailed(e)) => Err(H1ReqmodAdaptationError::IcapServerWriteFailed(e)),
                    };
                }
                r = self.icap_reader.fill_wait_data() => {
                    return match r {
                        Ok(true) => self.recv_icap_response().await,
                        Ok(false) => Err(H1ReqmodAdaptationError::IcapServerConnectionClosed),
                        Err(e) => Err(H1ReqmodAdaptationError::IcapServerReadFailed(e)),
                    };
                }
                n = idle_interval.tick() => {
                    if body_transfer.is_idle() {
                        idle_count += n;

                        let quit = self.idle_checker.check_quit(idle_count);
                        if quit {
                            return if body_transfer.no_cached_data() {
                                Err(H1ReqmodAdaptationError::HttpClientReadIdle)
                            } else {
                                Err(H1ReqmodAdaptationError::IcapServerWriteIdle)
                            };
                        }
                    } else {
                        idle_count = 0;

                        body_transfer.reset_active();
                    }

                    if let Some(reason) = self.idle_checker.check_force_quit() {
                        return Err(H1ReqmodAdaptationError::IdleForceQuit(reason));
                    }
                }
            }
        }
    }

    async fn recv_icap_response(self) -> Result<ReqmodResponse, H1ReqmodAdaptationError> {
        let rsp = ReqmodResponse::parse(
            self.icap_reader,
            self.icap_client.config.icap_max_header_size,
            &self.icap_client.config.respond_shared_names,
        )
        .await?;
        Ok(rsp)
    }
}

pub(super) struct BidirectionalRecvHttpRequest<'a, I: IdleCheck> {
    pub(super) http_body_line_max_size: usize,
    pub(super) http_req_add_no_via_header: bool,
    pub(super) copy_config: StreamCopyConfig,
    pub(super) idle_checker: &'a I,
    pub(crate) http_header_size: usize,
    pub(crate) icap_read_finished: bool,
}

impl<I: IdleCheck> BidirectionalRecvHttpRequest<'_, I> {
    pub(super) async fn transfer<H, CR, UW>(
        &mut self,
        state: &mut ReqmodAdaptationRunState,
        clt_body_transfer: &mut H1BodyToChunkedTransfer<'_, CR, IcapClientWriter>,
        orig_http_request: &H,
        icap_reader: &mut IcapClientReader,
        ups_writer: &mut UW,
    ) -> Result<ReqmodAdaptationEndState<H>, H1ReqmodAdaptationError>
    where
        H: HttpRequestForAdaptation,
        CR: AsyncBufRead + Unpin,
        UW: HttpRequestUpstreamWriter<H> + Unpin,
    {
        let http_req = HttpAdaptedRequest::parse(
            icap_reader,
            self.http_header_size,
            self.http_req_add_no_via_header,
        )
        .await?;
        let body_content_length = http_req.content_length;

        let final_req = orig_http_request.adapt_with_body(http_req);
        ups_writer
            .send_request_header(&final_req)
            .await
            .map_err(H1ReqmodAdaptationError::HttpUpstreamWriteFailed)?;
        state.mark_ups_send_header();

        match body_content_length {
            Some(0) => Err(H1ReqmodAdaptationError::InvalidHttpBodyFromIcapServer(
                anyhow!("Content-Length is 0 but the ICAP server response contains http-body"),
            )),
            Some(expected) => {
                let mut ups_body_reader =
                    HttpBodyDecodeReader::new_chunked(icap_reader, self.http_body_line_max_size);
                let mut ups_body_transfer =
                    StreamCopy::new(&mut ups_body_reader, ups_writer, &self.copy_config);
                self.do_transfer(clt_body_transfer, &mut ups_body_transfer)
                    .await?;

                state.mark_ups_send_all();
                let copied = ups_body_transfer.copied_size();
                if ups_body_reader.trailer(128).await.is_ok() {
                    self.icap_read_finished = true;
                }

                if copied != expected {
                    return Err(H1ReqmodAdaptationError::InvalidHttpBodyFromIcapServer(
                        anyhow!("Content-Length is {expected} but decoded length is {copied}"),
                    ));
                }
                Ok(ReqmodAdaptationEndState::AdaptedTransferred(final_req))
            }
            None => {
                let mut ups_body_reader =
                    HttpBodyReader::new_chunked(icap_reader, self.http_body_line_max_size);
                let mut ups_body_transfer =
                    StreamCopy::new(&mut ups_body_reader, ups_writer, &self.copy_config);
                self.do_transfer(clt_body_transfer, &mut ups_body_transfer)
                    .await?;

                state.mark_ups_send_all();
                self.icap_read_finished = ups_body_transfer.finished();

                Ok(ReqmodAdaptationEndState::AdaptedTransferred(final_req))
            }
        }
    }

    async fn do_transfer<CR, IR, UW>(
        &self,
        mut clt_body_transfer: &mut H1BodyToChunkedTransfer<'_, CR, IcapClientWriter>,
        mut ups_body_transfer: &mut StreamCopy<'_, IR, UW>,
    ) -> Result<(), H1ReqmodAdaptationError>
    where
        CR: AsyncBufRead + Unpin,
        IR: AsyncRead + Unpin,
        UW: AsyncWrite + Unpin,
    {
        let mut idle_interval = self.idle_checker.interval_timer();
        let mut idle_count = 0;

        loop {
            tokio::select! {
                r = &mut clt_body_transfer => {
                    return match r {
                        Ok(_) => {
                            match ups_body_transfer.await {
                                Ok(_) => Ok(()),
                                Err(StreamCopyError::ReadFailed(e)) => Err(H1ReqmodAdaptationError::IcapServerReadFailed(e)),
                                Err(StreamCopyError::WriteFailed(e)) => Err(H1ReqmodAdaptationError::HttpUpstreamWriteFailed(e)),
                            }
                        }
                        Err(StreamCopyError::ReadFailed(e)) => Err(H1ReqmodAdaptationError::HttpClientReadFailed(e)),
                        Err(StreamCopyError::WriteFailed(e)) => Err(H1ReqmodAdaptationError::IcapServerWriteFailed(e)),
                    };
                }
                r = &mut ups_body_transfer => {
                    return match r {
                        Ok(_) => Ok(()),
                        Err(StreamCopyError::ReadFailed(e)) => Err(H1ReqmodAdaptationError::IcapServerReadFailed(e)),
                        Err(StreamCopyError::WriteFailed(e)) => Err(H1ReqmodAdaptationError::HttpUpstreamWriteFailed(e)),
                    };
                }
                n = idle_interval.tick() => {
                    if clt_body_transfer.is_idle() && ups_body_transfer.is_idle() {
                        idle_count += n;

                        let quit = self.idle_checker.check_quit(idle_count);
                        if quit {
                            return if clt_body_transfer.is_idle() {
                                if clt_body_transfer.no_cached_data() {
                                    Err(H1ReqmodAdaptationError::HttpClientReadIdle)
                                } else {
                                    Err(H1ReqmodAdaptationError::IcapServerWriteIdle)
                                }
                            } else if ups_body_transfer.no_cached_data() {
                                Err(H1ReqmodAdaptationError::IcapServerReadIdle)
                            } else {
                                Err(H1ReqmodAdaptationError::HttpUpstreamWriteIdle)
                            };
                        }
                    } else {
                        idle_count = 0;

                        clt_body_transfer.reset_active();
                        ups_body_transfer.reset_active();
                    }

                    if let Some(reason) = self.idle_checker.check_force_quit() {
                        return Err(H1ReqmodAdaptationError::IdleForceQuit(reason));
                    }
                }
            }
        }
    }
}
