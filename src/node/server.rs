use std::io::Error;
use std::net::SocketAddrV4;
use std::os::fd::RawFd;
use std::thread::{self, sleep};
use std::time::Instant;
use log::{debug, error, info, trace, warn};

use io_uring::types::RecvMsgOut;
use io_uring::buf_ring::Buf;
use io_uring::CompletionQueue; 
use crate::io_uring::multishot::IoUringMultishot;
use crate::io_uring::normal::IoUringNormal;
use crate::util::msghdr_vec::MsghdrVec;
use crate::util::packet_buffer::PacketBuffer;
use crate::net::{socket::Socket, MessageHeader, MessageType};
use crate::util::{self, statistic::*, ExchangeFunction, IOModel};
use super::Node;

const INITIAL_POLL_TIMEOUT: i32 = 10000; // in milliseconds
const IN_MEASUREMENT_POLL_TIMEOUT: i32 = 1000; // in milliseconds

pub struct Server {
    packet_buffer: PacketBuffer,
    socket: Socket,
    io_uring_sqpoll_fd: Option<RawFd>,
    next_packet_id: u64,
    parameter: Parameter,
    measurements: Vec<Measurement>,
    exchange_function: ExchangeFunction
}

impl Server {
    pub fn new(sock_address_in: SocketAddrV4, socket: Option<Socket>, io_uring: Option<RawFd>, parameter: Parameter) -> Server {
        let socket = if let Some(socket) = socket {
            socket
        } else {
            let mut socket: Socket = Socket::new(parameter.socket_options).expect("Error creating socket");
            socket.bind(sock_address_in).expect("Error binding to local port");
            socket
        };

        info!("Current mode 'server' listening on {}:{} with socketID {}", sock_address_in.ip(), sock_address_in.port(), socket.get_socket_id());
        let packet_buffer = PacketBuffer::new(MsghdrVec::new(parameter.packet_buffer_size, parameter.mss, parameter.datagram_size as usize).with_cmsg_buffer());

        Server {
            packet_buffer,
            socket,
            io_uring_sqpoll_fd: io_uring,
            next_packet_id: 0,
            parameter,
            measurements: Vec::new(),
            exchange_function: parameter.exchange_function
        }
    }

    fn recv_messages(&mut self) -> Result<(), &'static str> {
        // Normally, we need to reset the msg_controllen field to the buffer size of all msghdr structs, since the kernel overwrites the value on return.
        // The same applies to the msg_flags field, which is set by the kernel in the msghdr struct.
        // To safe performance, we don't reset the fields, and ignore the msg_flags.
        // The msg_controllen field should be the same for all messages, since it should only contain the GRO enabled control message.
        // It is only reset after the first message, since the first message is the INIT message, which doesn't contain any control messages.

        if self.parameter.socket_options.gro && self.next_packet_id == 0 {
            self.packet_buffer.reset_msghdr_fields();
        }

        match self.exchange_function {
            ExchangeFunction::Normal => self.recv(),
            ExchangeFunction::Msg => self.recvmsg(),
            ExchangeFunction::Mmsg => self.recvmmsg(),
        }
    }

    fn recv(&mut self) -> Result<(), &'static str> {
        // Only one buffer is used, so we can directly access the first element
        let buffer_pointer = self.packet_buffer.get_buffer_pointer_from_index(0).unwrap();

        match self.socket.recv(buffer_pointer) {
            Ok(amount_received_bytes) => {
                let test_id = MessageHeader::get_test_id(buffer_pointer) as usize;
                let mtype = MessageHeader::get_message_type(buffer_pointer);
                debug!("Received packet with test id: {}", test_id);

                Self::parse_message_type(mtype, test_id, &mut self.measurements, self.parameter)?;

                let statistic = &mut self.measurements.get_mut(test_id).expect("Error getting statistic: test id not found").statistic;
                let datagram_size = self.packet_buffer.datagram_size();
                let amount_received_packets = util::process_packet_buffer(self.packet_buffer.get_buffer_pointer_from_index(0).unwrap(), datagram_size, self.next_packet_id, statistic);
                self.next_packet_id += amount_received_packets;
                statistic.amount_datagrams += amount_received_packets;
                statistic.amount_data_bytes += amount_received_bytes;
                Ok(())
            },
            Err(x) => Err(x)
        }
    }

    fn recvmsg(&mut self) -> Result<(), &'static str> {
        // Only one buffer is used, so we can directly access the first element
        let msghdr = self.packet_buffer.get_msghdr_from_index(0).unwrap();

        match self.socket.recvmsg(msghdr) {
            Ok(amount_received_bytes) => {
                let buffer_pointer = self.packet_buffer.get_buffer_pointer_from_index(0).unwrap();
                let test_id = MessageHeader::get_test_id(buffer_pointer) as usize;
                let mtype = MessageHeader::get_message_type(buffer_pointer);
        
                Self::parse_message_type(mtype, test_id, &mut self.measurements, self.parameter)?;
        
                let msghdr = self.packet_buffer.get_msghdr_from_index(0).unwrap();
                let statistic = &mut self.measurements.get_mut(test_id).expect("Error getting statistic: test id not found").statistic;
                let absolut_packets_received;
                (self.next_packet_id, absolut_packets_received) = util::process_packet_msghdr(msghdr, amount_received_bytes, self.next_packet_id, statistic);
                statistic.amount_datagrams += absolut_packets_received;
                statistic.amount_data_bytes += amount_received_bytes;
                debug!("Received {} packets and total {} Bytes, and next packet id should be {}", absolut_packets_received, amount_received_bytes, self.next_packet_id);
                Ok(())
            },
            Err(x) => Err(x)
        }
    }

    fn handle_recvmsg_return(&mut self, amount_received_bytes: i32,  msghdr: Option<&mut libc::msghdr>, msghdr_index: u64) -> Result<(), &'static str> {
        let msghdr = match msghdr {
            Some(msghdr) => msghdr,
            None => self.packet_buffer.get_msghdr_from_index(msghdr_index as usize).unwrap()
        };

        let libc::iovec { iov_base, iov_len } = unsafe {*msghdr.msg_iov};
        let buffer_pointer = unsafe {
            // Get buffer from iov_base with type &[u8]
            std::slice::from_raw_parts(iov_base as *const u8, iov_len )
        };
        
        let test_id = MessageHeader::get_test_id(buffer_pointer) as usize;
        let mtype = MessageHeader::get_message_type(buffer_pointer);

        Self::parse_message_type(mtype, test_id, &mut self.measurements, self.parameter)?;

        let msghdr = if self.parameter.uring_parameter.provided_buffer {
           msghdr
        } else {
            self.packet_buffer.get_msghdr_from_index(msghdr_index as usize).unwrap()
        };


        let statistic = &mut self.measurements.get_mut(test_id).expect("Error getting statistic: test id not found").statistic;
        let absolut_packets_received;
        (self.next_packet_id, absolut_packets_received) = util::process_packet_msghdr(msghdr, amount_received_bytes as usize, self.next_packet_id, statistic);
        statistic.amount_datagrams += absolut_packets_received;
        statistic.amount_data_bytes += amount_received_bytes as usize;
        debug!("Received {} packets and total {} Bytes, and next packet id should be {}", absolut_packets_received, amount_received_bytes, self.next_packet_id);

        Ok(())
    }

    fn recvmmsg(&mut self) -> Result<(), &'static str> {
        match self.socket.recvmmsg(&mut self.packet_buffer.mmsghdr_vec) {
            Ok(amount_received_mmsghdr) => { 
                if amount_received_mmsghdr == 0 {
                    debug!("No packets received during this recvmmsg call");
                    return Ok(());
                }

                let test_id = MessageHeader::get_test_id(self.packet_buffer.get_buffer_pointer_from_index(0).unwrap()) as usize;
                let mtype = MessageHeader::get_message_type(self.packet_buffer.get_buffer_pointer_from_index(0).unwrap());
                let amount_received_bytes = util::get_total_bytes(&self.packet_buffer.mmsghdr_vec, amount_received_mmsghdr);

                Self::parse_message_type(mtype, test_id, &mut self.measurements, self.parameter)?;

                let statistic = &mut self.measurements.get_mut(test_id).expect("Error getting statistic: test id not found").statistic;
                let mut absolut_datagrams_received = 0;

                // Check and calculate the amount of received packets and bytes
                for (index, mmsghdr) in self.packet_buffer.mmsghdr_vec.iter_mut().enumerate() {
                    if index >= amount_received_mmsghdr {
                        break;
                    }
                    let msghdr = &mut mmsghdr.msg_hdr;
                    let msghdr_bytes = mmsghdr.msg_len as usize;

                    let datagrams_received;
                    (self.next_packet_id, datagrams_received) = util::process_packet_msghdr(msghdr, msghdr_bytes, self.next_packet_id, statistic);
                    absolut_datagrams_received += datagrams_received;
                }

                statistic.amount_datagrams += absolut_datagrams_received;
                statistic.amount_data_bytes += amount_received_bytes;
                trace!("Sent {} msg_hdr to remote host", amount_received_mmsghdr);
                Ok(())
            },
            Err(x) => Err(x)
        }
    }

    fn parse_message_type(mtype: MessageType, test_id: usize, measurements: &mut Vec<Measurement>, parameter: Parameter) -> Result<(), &'static str> {
        match mtype {
            MessageType::INIT => {
                info!("{:?}: INIT packet received from test {}!", thread::current().id(), test_id);
                // Resize the vector if neeeded, and create a new measurement struct
                if measurements.len() <= test_id {
                    measurements.resize(test_id + 1, Measurement::new(parameter));
                }
                Err("INIT_MESSAGE_RECEIVED")
            },
            MessageType::MEASUREMENT => { 
                let measurement = if let Some(x) = measurements.get_mut(test_id) {
                    x
                } else {
                    // No INIT message received before, so we need to resize and create/add a new measurement struct
                    if measurements.len() <= test_id {
                        measurements.resize(test_id + 1, Measurement::new(parameter));
                    }
                    measurements.get_mut(test_id).expect("Error getting statistic in measurement message: test id not found")
                };
                // Start measurement timer with receiving of the first MEASUREMENT message
                if !measurement.first_packet_received {
                    info!("{:?}: First packet received from test {}!", thread::current().id(), test_id);
                    measurement.start_time = Instant::now();
                    measurement.first_packet_received = true;
                }
                Ok(())
            },
            MessageType::LAST => {
                info!("{:?}: LAST packet received from test {}!", thread::current().id(), test_id);
                // Not checking for measurement length anymore, since we assume that the thread has received at least one MEASUREMENT message before
                let measurement = measurements.get_mut(test_id).expect("Error getting statistic in last message: test id not found");
                let end_time = Instant::now() - std::time::Duration::from_millis(crate::WAIT_CONTROL_MESSAGE); // REMOVE THIS, if you remove the sleep in the client, before sending last message, as well
                measurement.last_packet_received = true;
                measurement.statistic.set_test_duration(measurement.start_time, end_time);
                measurement.statistic.calculate_statistics();
                Err("LAST_MESSAGE_RECEIVED")
            }
        }
    }


    fn io_uring_complete_multishot(&mut self,  io_uring_instance: &mut IoUringMultishot, statistic: &mut Statistic) -> Result<bool, &'static str> {
        let mut completion_count = 0;
        let mut multishot_armed = true;
        let msghdr = &io_uring_instance.get_msghdr();
        let (buf_ring, cq) = io_uring_instance.get_bufs_and_cq();
        let mut bufs = buf_ring.submissions();

        debug!("BEGIN io_uring_complete: Current cq len: {}. Dropped messages: {}", cq.len(), cq.overflow());

        if cq.overflow() > 0 {
            warn!("Dropped messages in completion queue: {}", cq.overflow());
        }

        for cqe in cq {
            let amount_received_bytes = cqe.result();
            debug!("Received completion event with bytes: {}", amount_received_bytes); 

            completion_count += 1;

            // Same as in socket.recvmsg function: Check if result is negative, and handle the error
            // Errors are negated, since a positive amount of bytes received is a success
            match amount_received_bytes {
                0 => {
                    warn!("Received empty message");
                    continue;
                },
                -105 => { // result is -105, libc::ENOBUFS, no buffer space available (https://github.com/tokio-rs/io-uring/blob/b29e81583ed9a2c35feb1ba6f550ac1abf398f48/src/squeue.rs#L87) TODO: Only needed for provided buffers
                    warn!("ENOBUFS: No buffer space available! Next expected packet ID: {}", self.next_packet_id);
                    return Ok(crate::io_uring::check_multishot_status(cqe.flags()));
                },
                _ if amount_received_bytes < 0 => {
                    let errno = Error::last_os_error();
                    match errno.raw_os_error() {
                        // If no messages are available at the socket, the receive calls wait for a message to arrive, unless the socket is nonblocking (see fcntl(2)), in which case the value -1 is returned and the external variable errno is set to EAGAIN or EWOULDBLOCK.
                        // At the moment, this doesn't happen with nonblocking sockets in io_uring.
                        // From: https://linux.die.net/man/2/recvmsg
                        Some(libc::EAGAIN) => { 
                            warn!("EAGAIN"); 
                            return Err("EAGAIN"); 
                        },
                        _ => {
                            error!("Error receiving message: {}", errno);
                            return Err("Failed to receive data!");
                        } 
                    }
                },
                _ => {} // Positive amount of bytes received
            }

            multishot_armed = crate::io_uring::check_multishot_status(cqe.flags()); 

            // Get specific buffer from the buffer ring
            let buf = unsafe {
                bufs.get(cqe.flags(), usize::try_from(amount_received_bytes).unwrap())
            };

            // Helps parsing buffer of multishot recvmsg
            // https://docs.rs/io-uring/latest/io_uring/types/struct.RecvMsgOut.html
            // https://github.com/SUPERCILEX/clipboard-history/blob/95bae326388d7f6f4a63fead5eca4851fd2de1c8/server/src/reactor.rs#L211
            let msg = RecvMsgOut::parse(&buf, msghdr).expect("Parsing of RecvMsgOut failed. Didn't allocate large enough buffers");
            trace!("Received message: {:?}", msg);

            // Check if any data is truncated
            if msg.is_control_data_truncated() {
                debug!("The control data was truncated");
            } else if msg.is_payload_truncated() {
                debug!("The payload was truncated");
            } else if msg.is_name_data_truncated() {
                debug!("The name data was truncated");
            }

            statistic.uring_canceled_multishot += if multishot_armed { 0 } else { 1 };

            // Build iovec struct for recvmsg to reuse handle_recvmsg_return code
            let iovec: libc::iovec = libc::iovec {
                iov_base: msg.payload_data().as_ptr() as *mut libc::c_void,
                iov_len: (amount_received_bytes - crate::URING_ADDITIONAL_BUFFER_LENGTH) as usize
            };

            let mut msghdr: libc::msghdr = {
                let mut hdr = unsafe { std::mem::zeroed::<libc::msghdr>() };
                hdr.msg_iov = &iovec as *const _ as *mut _;
                hdr.msg_iovlen = 1;
                hdr.msg_control = msg.control_data().as_ptr() as *mut libc::c_void;
                hdr.msg_controllen = msg.control_data().len();
                hdr
            };
            
            // Parse recvmsg msghdr on return
            // TODO: Should do the same (AND return the same errors) as the normal recvmsg function.
            // TODO: Struct to catch this should be the same as the match block from original recv_messages loop
            // Maybe when using multishot recvmsg, we can add an own io_uring function to recv_messages() and use the same loop
            match self.handle_recvmsg_return(amount_received_bytes - crate::URING_ADDITIONAL_BUFFER_LENGTH,  Some(&mut msghdr), 0) {
                Ok(_) => {},
                Err("INIT_MESSAGE_RECEIVED") => break,
                Err("LAST_MESSAGE_RECEIVED") => {
                    for measurement in self.measurements.iter() {
                        if !measurement.last_packet_received && measurement.first_packet_received {
                            debug!("{:?}: Last message received, but not all measurements are finished!", thread::current().id());
                        } 
                    };
                    info!("{:?}: Last message received and all measurements are finished!", thread::current().id());
                    return Err("LAST_MESSAGE_RECEIVED");
                },
                Err(x) => {
                    error!("Error receiving message! Aborting measurement...");
                    return Err(x);
                }
            }
        }

        debug!("END io_uring_complete: Completed {} io_uring cqe. Multishot is still armed: {}", completion_count, multishot_armed);
        Ok(multishot_armed)
    }


    fn io_uring_complete(&mut self, cq: &mut CompletionQueue) -> Result<u32, &'static str> {
        let mut completion_count = 0;

        debug!("BEGIN io_uring_complete: Current cq len: {}. Dropped messages: {}", cq.len(), cq.overflow());

        if cq.overflow() > 0 {
            warn!("Dropped messages in completion queue: {}", cq.overflow());
        }

        // Pool to store the buffer indexes, which are used in the completion queue to return them later
        let mut index_pool: Vec<usize> = Vec::with_capacity(cq.len());

        // Drain completion queue events
        for cqe in cq {
            let amount_received_bytes = cqe.result();
            let user_data = cqe.user_data();
            debug!("Received completion event with user_data: {}, and received bytes: {}", user_data, amount_received_bytes); 

            // Same as in socket.recvmsg function: Check if result is negative, and handle the error
            // Errors are negated, since a positive amount of bytes received is a success
            match amount_received_bytes {
                0 => {
                    warn!("Received empty message");
                    completion_count += 1;
                    continue;
                },
                -105 => { // result is -105, libc::ENOBUFS, no buffer space available (https://github.com/tokio-rs/io-uring/blob/b29e81583ed9a2c35feb1ba6f550ac1abf398f48/src/squeue.rs#L87) TODO: Only needed for provided buffers
                    debug!("ENOBUFS: No buffer space available! (Next expected packet ID; {}", self.next_packet_id);
                    continue;
                },
                -11 => {
                    // If no messages are available at the socket, the receive calls wait for a message to arrive, unless the socket is nonblocking (see fcntl(2)), in which case the value -11 is returned and the external variable errno is set to EAGAIN or EWOULDBLOCK.
                    // From: https://linux.die.net/man/2/recvmsg
                    // libc::EAGAIN == 11
                    return Err("EAGAIN");
                },
                _ if amount_received_bytes < 0 => {
                    let errno = Error::last_os_error();
                    error!("Error receiving message: {}", errno);
                    return Err("Failed to receive data");
                },
                _ => { // Positive amount of bytes received
                    completion_count += 1;
                }
            }

            // Parse recvmsg msghdr on return
            // TODO: Should do the same (AND return the same errors) as the normal recvmsg function.
            // TODO: Struct to catch this should be the same as the match block from original recv_messages loop
            // Maybe when using multishot recvmsg, we can add an own io_uring function to recv_messages() and use the same loop
            match self.handle_recvmsg_return(amount_received_bytes, None, user_data) {
                Ok(_) => {},
                Err("INIT_MESSAGE_RECEIVED") => {
                    index_pool.push(user_data as usize);
                    continue;
                },
                Err("LAST_MESSAGE_RECEIVED") => {
                    for measurement in self.measurements.iter() {
                        if !measurement.last_packet_received && measurement.first_packet_received {
                            debug!("{:?}: Last message received, but not all measurements are finished!", thread::current().id());
                        } 
                    };
                    info!("{:?}: Last message received and all measurements are finished!", thread::current().id());
                    return Err("LAST_MESSAGE_RECEIVED");
                },
                Err(x) => {
                    error!("Error receiving message! Aborting measurement...");
                    return Err(x);
                }
            }

            index_pool.push(user_data as usize);
        }

        // Returns used buffers to the buffer ring.
        self.packet_buffer.return_buffer_index(index_pool);

        debug!("END io_uring_complete: Completed {} io_uring cqe", completion_count);
        Ok(completion_count)
    }


    fn io_uring_complete_provided_buffers(&mut self, io_uring_instance: &mut IoUringNormal) -> Result<u32, &'static str> {
        let mut completion_count = 0;
        //let mut bufs = if self.parameter.uring_parameter.provided_buffer {
        //    buf_ring.as_mut().unwrap().submissions()
        //} else {
        //    return Err("Provided buffers not available");
        //};
        let (buf_ring, cq) = io_uring_instance.get_bufs_and_cq();
        let mut bufs = buf_ring.submissions();

        debug!("BEGIN io_uring_complete: Current cq len: {}. Dropped messages: {}", cq.len(), cq.overflow());

        if cq.overflow() > 0 {
            warn!("Dropped messages in completion queue: {}", cq.overflow());
        }

        // Drain completion queue events
        for cqe in cq {
            let amount_received_bytes = cqe.result();
            let user_data = cqe.user_data();
            debug!("Received completion event with user_data: {}, and received bytes: {}", user_data, amount_received_bytes); 

            // Same as in socket.recvmsg function: Check if result is negative, and handle the error
            // Errors are negated, since a positive amount of bytes received is a success
            match amount_received_bytes {
                0 => {
                    warn!("Received empty message");
                    completion_count += 1;
                    continue;
                },
                -105 => { // result is -105, libc::ENOBUFS, no buffer space available (https://github.com/tokio-rs/io-uring/blob/b29e81583ed9a2c35feb1ba6f550ac1abf398f48/src/squeue.rs#L87) TODO: Only needed for provided buffers
                    debug!("ENOBUFS: No buffer space available! (Next expected packet ID; {}", self.next_packet_id);
                    continue;
                },
                -11 => {
                    // If no messages are available at the socket, the receive calls wait for a message to arrive, unless the socket is nonblocking (see fcntl(2)), in which case the value -11 is returned and the external variable errno is set to EAGAIN or EWOULDBLOCK.
                    // From: https://linux.die.net/man/2/recvmsg
                    // libc::EAGAIN == 11
                    return Err("EAGAIN");
                },
                _ if amount_received_bytes < 0 => {
                    let errno = Error::last_os_error();
                    error!("Error receiving message: {}", errno);
                    return Err("Failed to receive data");
                },
                _ => { // Positive amount of bytes received
                    completion_count += 1;
                }
            }

            let iovec: libc::iovec;
            let mut msghdr: libc::msghdr;
            let mut buf: Buf; 

            let pmsghdr  = {
                // Following lines are only provided buffers specific
                // Get specific buffer from the buffer ring
                buf = unsafe {
                    bufs.get(cqe.flags(), usize::try_from(amount_received_bytes).unwrap())
                };

                // Build iovec struct for recvmsg to reuse handle_recvmsg_return code
                iovec = libc::iovec {
                    iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                    iov_len: amount_received_bytes as usize
                };

                msghdr = {
                    let mut hdr = unsafe { std::mem::zeroed::<libc::msghdr>() };
                    hdr.msg_iov = &iovec as *const _ as *mut _;
                    hdr.msg_iovlen = 1;
                    hdr
                };
                &mut msghdr
            };

            // Parse recvmsg msghdr on return
            // TODO: Should do the same (AND return the same errors) as the normal recvmsg function.
            // TODO: Struct to catch this should be the same as the match block from original recv_messages loop
            // Maybe when using multishot recvmsg, we can add an own io_uring function to recv_messages() and use the same loop
            match self.handle_recvmsg_return(amount_received_bytes, Some(pmsghdr), user_data) {
                Ok(_) => {},
                Err("INIT_MESSAGE_RECEIVED") => {
                    continue;
                },
                Err("LAST_MESSAGE_RECEIVED") => {
                    for measurement in self.measurements.iter() {
                        if !measurement.last_packet_received && measurement.first_packet_received {
                            debug!("{:?}: Last message received, but not all measurements are finished!", thread::current().id());
                        } 
                    };
                    info!("{:?}: Last message received and all measurements are finished!", thread::current().id());
                    return Err("LAST_MESSAGE_RECEIVED");
                },
                Err(x) => {
                    error!("Error receiving message! Aborting measurement...");
                    return Err(x);
                }
            }
        }

        debug!("END io_uring_complete: Completed {} io_uring cqe", completion_count);
        Ok(completion_count)
    }


    fn io_uring_loop(&mut self, mut statistic: Statistic) -> Result<Statistic, &'static str> {
        let socket_fd = self.socket.get_socket_id();

        if self.parameter.uring_parameter.multishot {
            let mut io_uring_instance = crate::io_uring::multishot::IoUringMultishot::new(self.parameter, self.io_uring_sqpoll_fd)?;
            // Indicator if multishot request is still armed
            let mut armed = false;

            loop {
                io_uring_instance.fill_sq_and_submit(armed, socket_fd)?;

                match self.io_uring_complete_multishot(&mut io_uring_instance, &mut statistic) {
                    Ok(multishot_armed) => {
                        armed = multishot_armed
                    },
                    Err("LAST_MESSAGE_RECEIVED") => {
                        return Ok(statistic + io_uring_instance.get_statistic());
                        // TODO: Check if all measurements are done
                    },
                    Err(x) => {
                        error!("Error completing io_uring sqe: {}", x);
                        return Err(x);
                    }
                };

                statistic.amount_io_model_calls += 1;
            }
        } else {
            let mut io_uring_instance = crate::io_uring::normal::IoUringNormal::new(self.parameter, self.io_uring_sqpoll_fd)?;
            let mut amount_inflight = 0;

            loop {
                amount_inflight += io_uring_instance.fill_sq_and_submit(amount_inflight, &mut self.packet_buffer, socket_fd)?;

                match if self.parameter.uring_parameter.provided_buffer { self.io_uring_complete_provided_buffers(&mut io_uring_instance) } else { self.io_uring_complete(&mut io_uring_instance.get_cq()) } {
                    Ok(completed) => {
                        amount_inflight -= completed
                    },
                    Err("LAST_MESSAGE_RECEIVED") => {
                        return Ok(statistic + io_uring_instance.get_statistic());
                        // TODO: Check if all measurements are done
                    },
                    Err("EAGAIN") => continue,
                    Err(x) => {
                        error!("Error completing io_uring sqe: {}", x);
                        return Err(x);
                    }
                };
    
                statistic.uring_inflight_utilization[amount_inflight as usize] += 1;
                statistic.amount_io_model_calls += 1;
            }
        }
    }
}


impl Node for Server { 
    fn run(&mut self, io_model: IOModel) -> Result<Statistic, &'static str> {
        info!("Start server loop...");
        let mut statistic = Statistic::new(self.parameter);

        // Timeout waiting for first message 
        // With communication channel in future, the measure thread is only started if the client starts a measurement. Then timeout can be further reduced to 1-2s.
        let mut pollfd = self.socket.create_pollfd(libc::POLLIN);
        match self.socket.poll(&mut pollfd, INITIAL_POLL_TIMEOUT) {
            Ok(_) => {},
            Err("TIMEOUT") => {
                // If port sharding is used, not every server thread gets packets due to the load balancing of REUSEPORT.
                // To avoid that the thread waits forever, we need to return here.
                error!("{:?}: Timeout waiting for client to send first packet!", thread::current().id());
                return Ok(statistic);
            },
            Err(x) => {
                return Err(x);
            }
        };

        // TODO: Very ugly at the moment
        if io_model == IOModel::IoUring {
            statistic = self.io_uring_loop(statistic)?;
        } else {
            'outer: loop {
                match self.recv_messages() {
                    Ok(_) => {},
                    Err("EAGAIN") => {
                        statistic.amount_io_model_calls += 1;
                        match self.io_wait(io_model) {
                            Ok(_) => {},
                            Err("TIMEOUT") => {
                                // If port sharing is used, or single connection not every thread receives the LAST message. 
                                // To avoid that the thread waits forever, we need to return here.
                                error!("{:?}: Timeout waiting for a subsequent packet from the client!", thread::current().id());
                                break 'outer;
                            },
                            Err(x) => {
                                return Err(x);
                            }
                        }
                    },
                    Err("LAST_MESSAGE_RECEIVED") => {
                        for measurement in self.measurements.iter() {
                            if !measurement.last_packet_received && measurement.first_packet_received {
                                debug!("{:?}: Last message received, but not all measurements are finished!", thread::current().id());
                                continue 'outer;
                            } 
                        };
                        info!("{:?}: Last message received and all measurements are finished!", thread::current().id());
                        break 'outer;
                    },
                    Err("INIT_MESSAGE_RECEIVED") => {
                        continue 'outer;
                    },
                    Err(x) => {
                        error!("Error receiving message! Aborting measurement...");
                        return Err(x)
                    }
                }
                statistic.amount_syscalls += 1;
            }
        }

        if self.parameter.multiplex_port_server != MultiplexPort::Sharing {
            // If a thread finishes (closes the socket) before the others, the hash mapping of SO_REUSEPORT changes. 
            // Then all threads would receive packets from other connections (test_ids).
            // Therefore, we need to wait a bit, until a thread closes its socket.
            if self.parameter.multiplex_port_server == MultiplexPort::Sharding {
                sleep(std::time::Duration::from_millis(crate::WAIT_CONTROL_MESSAGE * 2));
            }
            self.socket.close()?;
        }

        debug!("{:?}: Finished receiving data from remote host", thread::current().id());
        // Fold over all statistics, and calculate the final statistic
        let statistic = self.measurements.iter().fold(statistic, |acc: Statistic, measurement| acc + measurement.statistic.clone());
        Ok(statistic)
    }

    fn io_wait(&mut self, io_model: IOModel) -> Result<(), &'static str> {
        // Normally we would need to iterate over FDs and check which socket is ready
        // Since we only have one socket, we directly call recv_messages after io_wait returns
        match io_model {
            IOModel::Select => {
                let mut read_fds: libc::fd_set = unsafe { self.socket.create_fdset() };
                self.socket.select(Some(&mut read_fds), None, IN_MEASUREMENT_POLL_TIMEOUT)

            },
            IOModel::Poll => {
                let mut pollfd = self.socket.create_pollfd(libc::POLLIN);
                self.socket.poll(&mut pollfd, IN_MEASUREMENT_POLL_TIMEOUT)
            },
            _ => Ok(())
        }
    }
}
