/*
 * Copyright 2023 ByteDance and/or its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::io::{self, IoSlice, IoSliceMut};
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::fd::AsRawFd;
use std::task::{ready, Context, Poll};

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd"
))]
use nix::sys::socket::{recvmmsg, sendmmsg, MultiHeaders};
use nix::sys::socket::{recvmsg, sendmsg, MsgFlags, SockaddrStorage};
use tokio::io::Interest;
use tokio::net::UdpSocket;

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd"
))]
pub struct SendMsgHdr<'a, const C: usize> {
    pub iov: [IoSlice<'a>; C],
    pub addr: Option<SocketAddr>,
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd"
))]
impl<'a, const C: usize> AsRef<[IoSlice<'a>]> for SendMsgHdr<'a, C> {
    fn as_ref(&self) -> &[IoSlice<'a>] {
        self.iov.as_ref()
    }
}

#[derive(Clone, Copy, Default)]
pub struct RecvMsghdr {
    pub len: usize,
    pub addr: Option<SocketAddr>,
}

pub trait UdpSocketExt {
    fn poll_sendmsg(
        &self,
        cx: &mut Context<'_>,
        iov: &[IoSlice<'_>],
        target: Option<SocketAddr>,
    ) -> Poll<io::Result<usize>>;

    fn poll_recvmsg(
        &self,
        cx: &mut Context<'_>,
        iov: &mut IoSliceMut<'_>,
    ) -> Poll<io::Result<RecvMsghdr>>;

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "netbsd"
    ))]
    fn poll_batch_sendmsg<const C: usize>(
        &self,
        cx: &mut Context<'_>,
        msgs: &[SendMsgHdr<'_, C>],
    ) -> Poll<io::Result<usize>>;

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "netbsd"
    ))]
    fn poll_batch_recvmsg(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMsghdr],
    ) -> Poll<io::Result<usize>>;
}

impl UdpSocketExt for UdpSocket {
    fn poll_sendmsg(
        &self,
        cx: &mut Context<'_>,
        iov: &[IoSlice<'_>],
        target: Option<SocketAddr>,
    ) -> Poll<io::Result<usize>> {
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "netbsd"
        ))]
        let flags: MsgFlags = MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL;
        #[cfg(target_os = "macos")]
        let flags: MsgFlags = MsgFlags::MSG_DONTWAIT;

        let raw_fd = self.as_raw_fd();
        let addr = target.map(SockaddrStorage::from);

        loop {
            ready!(self.poll_send_ready(cx))?;
            match self.try_io(Interest::WRITABLE, || {
                sendmsg(raw_fd, iov, &[], flags, addr.as_ref()).map_err(io::Error::from)
            }) {
                Ok(res) => return Poll::Ready(Ok(res)),
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        continue;
                    }
                    return Poll::Ready(Err(e));
                }
            }
        }
    }

    fn poll_recvmsg(
        &self,
        cx: &mut Context<'_>,
        buf: &mut IoSliceMut<'_>,
    ) -> Poll<io::Result<RecvMsghdr>> {
        let flags: MsgFlags = MsgFlags::MSG_DONTWAIT;

        let raw_fd = self.as_raw_fd();
        let mut iov = [IoSliceMut::new(buf.as_mut())];

        loop {
            ready!(self.poll_recv_ready(cx))?;
            match self.try_io(Interest::READABLE, || {
                recvmsg::<SockaddrStorage>(raw_fd, &mut iov, None, flags).map_err(io::Error::from)
            }) {
                Ok(res) => {
                    let addr = res.address.and_then(|v| {
                        v.as_sockaddr_in()
                            .map(|v4| SocketAddr::V4(SocketAddrV4::from(*v4)))
                            .or_else(|| {
                                v.as_sockaddr_in6()
                                    .map(|v6| SocketAddr::V6(SocketAddrV6::from(*v6)))
                            })
                    });
                    let len = res.iovs().next().map(|b| b.len()).unwrap_or_default();
                    return Poll::Ready(Ok(RecvMsghdr { len, addr }));
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        continue;
                    }
                    return Poll::Ready(Err(e));
                }
            }
        }
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "netbsd"
    ))]
    fn poll_batch_sendmsg<const C: usize>(
        &self,
        cx: &mut Context<'_>,
        msgs: &[SendMsgHdr<'_, C>],
    ) -> Poll<io::Result<usize>> {
        let flags: MsgFlags = MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL;

        let mut data = MultiHeaders::<SockaddrStorage>::preallocate(msgs.len(), None);
        let addrs = msgs
            .iter()
            .map(|v| v.addr.map(SockaddrStorage::from))
            .collect::<Vec<Option<SockaddrStorage>>>();
        let raw_fd = self.as_raw_fd();

        loop {
            ready!(self.poll_send_ready(cx))?;
            match self.try_io(Interest::WRITABLE, || {
                sendmmsg(raw_fd, &mut data, msgs, &addrs, [], flags).map_err(io::Error::from)
            }) {
                Ok(res) => return Poll::Ready(Ok(res.count())),
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        continue;
                    }
                    return Poll::Ready(Err(e));
                }
            }
        }
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "netbsd"
    ))]
    fn poll_batch_recvmsg(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMsghdr],
    ) -> Poll<io::Result<usize>> {
        let flags: MsgFlags = MsgFlags::MSG_DONTWAIT;

        let mut data = MultiHeaders::<SockaddrStorage>::preallocate(bufs.len(), None);
        let slices: Vec<_> = bufs
            .iter_mut()
            .map(|b| [IoSliceMut::new(b.as_mut())])
            .collect();
        let raw_fd = self.as_raw_fd();

        loop {
            ready!(self.poll_recv_ready(cx))?;
            match self.try_io(Interest::READABLE, || {
                recvmmsg(raw_fd, &mut data, &slices, flags, None).map_err(io::Error::from)
            }) {
                Ok(res) => {
                    let mut count = 0;
                    for (hdr, v) in meta.iter_mut().zip(res) {
                        let addr = v.address.and_then(|v| {
                            v.as_sockaddr_in()
                                .map(|v4| SocketAddr::V4(SocketAddrV4::from(*v4)))
                                .or_else(|| {
                                    v.as_sockaddr_in6()
                                        .map(|v6| SocketAddr::V6(SocketAddrV6::from(*v6)))
                                })
                        });
                        hdr.addr = addr;
                        if let Some(s) = v.iovs().next() {
                            hdr.len = s.len();
                        }
                        count += 1;
                    }
                    return Poll::Ready(Ok(count));
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        continue;
                    }
                    return Poll::Ready(Err(e));
                }
            }
        }
    }
}