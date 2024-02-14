mod common;

// Test client send/sendmsg/sendmmsg with server send/sendmsg/sendmmsg in different combinations

#[test]
fn sendmsg_recvmsg() -> Result<(), Box<dyn std::error::Error>>{
    common::start_nperf_server(Some(vec!["--with-msg".to_string(), "--port".to_string(), "45001".to_string()]));

    let args = vec!["client", "--with-msg", "--port", "45001"];
    if let Some(x) = nperf::nPerf::new().set_args(args).exec() {
        assert!(x.amount_datagrams > 10000);
    };

    Ok(())
}

#[test]
fn sendmmsg_recvmsg() -> Result<(), Box<dyn std::error::Error>>{
    common::start_nperf_server(Some(vec!["--with-msg".to_string(), "--port".to_string(), "45002".to_string()]));

    let args = vec!["client", "--with-mmsg", "--port", "45002"];
    if let Some(x) = nperf::nPerf::new().set_args(args).exec() {
        assert!(x.amount_datagrams > 10000);
    };

    Ok(())
}

#[test]
fn sendmmsg_recvmmsg() -> Result<(), Box<dyn std::error::Error>>{
    common::start_nperf_server(Some(vec!["--with-mmsg".to_string(), "--port".to_string(), "45003".to_string()]));

    let args = vec!["client", "--with-mmsg", "--port", "45003"];
    if let Some(x) = nperf::nPerf::new().set_args(args).exec() {
        assert!(x.amount_datagrams > 10000);
    };

    Ok(())
}

#[test]
fn sendmsg_recvmmsg() -> Result<(), Box<dyn std::error::Error>>{
    common::start_nperf_server(Some(vec!["--with-mmsg".to_string(), "--port".to_string(), "45004".to_string()]));

    let args = vec!["client", "--with-msg", "--port", "45004"];
    if let Some(x) = nperf::nPerf::new().set_args(args).exec() {
        assert!(x.amount_datagrams > 10000);
    };

    Ok(())
}

