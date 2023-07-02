use std::time::Duration;

use async_speed_limit::Limiter;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_io_timeout::TimeoutStream;

use super::{copy_bidirectional, CopyBiError};

const UPLINK_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 2);
const DOWNLINK_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn timed_copy_bidirectional<A, B>(
    a: A,
    b: B,
    speed_limiter: Limiter,
) -> Result<(u64, u64), CopyBiError>
where
    A: AsyncRead + AsyncWrite + Send + 'static,
    B: AsyncRead + AsyncWrite + Send + 'static,
{
    let mut a = TimeoutStream::new(a);
    let mut b = TimeoutStream::new(b);

    a.set_read_timeout(Some(UPLINK_TIMEOUT));
    a.set_write_timeout(Some(DOWNLINK_TIMEOUT));
    b.set_read_timeout(Some(DOWNLINK_TIMEOUT));
    b.set_write_timeout(Some(UPLINK_TIMEOUT));

    let a = speed_limiter.limit(a);

    let mut a = Box::pin(a);
    let mut b = Box::pin(b);

    copy_bidirectional(&mut a, &mut b).await

    // let (a_r, a_w) = tokio::io::split(a);
    // let (b_r, b_w) = tokio::io::split(b);

    // let mut a_r = TimeoutReader::new(a_r);
    // a_r.set_timeout(Some(UPLINK_TIMEOUT));
    // let mut b_r = TimeoutReader::new(b_r);
    // b_r.set_timeout(Some(DOWNLINK_TIMEOUT));
    // let mut a_w = TimeoutWriter::new(a_w);
    // a_w.set_timeout(Some(DOWNLINK_TIMEOUT));
    // let mut b_w = TimeoutWriter::new(b_w);
    // b_w.set_timeout(Some(UPLINK_TIMEOUT));

    // let limiter = <Limiter>::new(speed_limit);
    // let a_r = limiter.clone().limit(a_r);
    // let b_r = limiter.limit(b_r);

    // let mut a_r = Box::pin(a_r);
    // let mut b_r = Box::pin(b_r);
    // let mut a_w = Box::pin(a_w);
    // let mut b_w = Box::pin(b_w);

    // let mut join_set = ScopedJoinSet::new();
    // join_set.spawn(async move {
    //     tokio::io::copy(&mut a_r, &mut b_w)
    //         .await
    //         .map(CopyResult::AToB)
    //         .map_err(CopyBiError::FromAToB)
    // });
    // join_set.spawn(async move {
    //     tokio::io::copy(&mut b_r, &mut a_w)
    //         .await
    //         .map(CopyResult::BToA)
    //         .map_err(CopyBiError::FromBToA)
    // });

    // let mut a_to_b = None;
    // let mut b_to_a = None;
    // while let Some(res) = join_set.join_next().await {
    //     let res = res.unwrap()?;
    //     match res {
    //         CopyResult::AToB(n) => {
    //             a_to_b = Some(n);
    //         }
    //         CopyResult::BToA(n) => {
    //             b_to_a = Some(n);
    //         }
    //     }
    // }
    // Ok((a_to_b.unwrap(), b_to_a.unwrap()))
}

// enum CopyResult {
//     AToB(u64),
//     BToA(u64),
// }

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[allow(clippy::read_zero_byte_vec)]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_timed_copy_bidirectional() {
        let msg_list: Arc<[_]> = vec![
            "hello world",
            "world hello",
"
Lorem ipsum dolor sit amet, consectetur adipiscing elit. Donec congue laoreet nibh, commodo sagittis augue malesuada in. Aenean at posuere urna, nec bibendum libero. Sed et sollicitudin quam, sed imperdiet quam. Proin sed aliquet libero, non volutpat ligula. Pellentesque elit nulla, bibendum quis ligula a, dictum placerat ex. Nulla turpis lacus, varius quis laoreet eu, cursus a mi. Sed vitae nisi vel metus rutrum pharetra. Phasellus volutpat ante vitae libero luctus lobortis. Suspendisse quis sodales elit, vitae tempor felis. Praesent eget nunc egestas, fermentum metus ut, aliquet est. Vestibulum auctor vulputate molestie. Nullam suscipit feugiat eleifend. Mauris ac elit sed risus condimentum lobortis. Fusce ut venenatis velit. Donec efficitur justo et lorem vestibulum malesuada. In urna sem, sollicitudin a neque non, mattis scelerisque dolor.

Aenean in semper tellus, sed laoreet nisl. Vestibulum euismod sem ipsum, eget vulputate ex commodo nec. Duis in tincidunt arcu. Nulla quis dolor neque. Vestibulum convallis, sapien et viverra aliquam, ex orci sollicitudin enim, sit amet vestibulum nisi nisl efficitur justo. Donec a vehicula eros. Nunc semper accumsan sem ac posuere. Phasellus commodo ipsum tortor, vitae vestibulum nibh mattis convallis. Proin tincidunt interdum tellus, non porttitor nulla. Phasellus id sagittis mi. Vivamus lobortis dolor eget sodales euismod. Integer at elit sit amet libero sagittis laoreet vitae eleifend justo.

Sed finibus urna ut tortor sodales, at hendrerit lorem sollicitudin. Vestibulum ornare bibendum mi mattis pretium. Quisque ac mattis felis. Donec interdum vel nunc vitae volutpat. Donec in rhoncus arcu, quis scelerisque lectus. Quisque in convallis est. Phasellus pellentesque porttitor est quis malesuada. Phasellus fermentum, est luctus laoreet luctus, eros sapien semper felis, eu porttitor risus nibh in nisi. Aliquam id lectus nisl. Nam malesuada laoreet faucibus. Quisque egestas nulla ac nisl vehicula cursus.

Morbi placerat lectus at volutpat faucibus. Maecenas malesuada, mauris malesuada blandit interdum, eros tellus hendrerit dui, id rutrum odio ante ac diam. Curabitur tempor lectus diam, at pharetra nulla semper sit amet. Sed sed euismod felis, id maximus ligula. Proin quis fermentum ligula. Etiam et est eget lectus pulvinar blandit. Cras ornare nisl neque, non mattis arcu accumsan sit amet. Nulla quis hendrerit ligula, id tempus erat. Nullam et congue dui, id tincidunt odio. Lorem ipsum dolor sit amet, consectetur adipiscing elit. Cras id eros dui. Fusce ac tortor urna. Morbi rhoncus efficitur fringilla. Phasellus blandit velit id eros laoreet vulputate.

Morbi vitae eleifend dui. Vestibulum lobortis commodo pellentesque. Suspendisse non faucibus felis, sit amet elementum sapien. Quisque aliquet mollis commodo. Integer placerat blandit tempor. Nulla aliquam dignissim lorem, et laoreet lacus ornare sit amet. Etiam non arcu tempus, imperdiet magna eget, mollis sapien. Donec eget velit vitae est vulputate scelerisque.
",
        ]
        .into();
        let epoch = 0x100;
        let parallel = 0x100;

        let mut join_set = tokio::task::JoinSet::new();
        for _ in 0..parallel {
            let msg_list = msg_list.clone();
            join_set.spawn(async move {
                // a_1, a_2, b_2, b_1
                let (a_1, a_2) = tokio::io::duplex(1024 * 64);
                let (b_1, b_2) = tokio::io::duplex(1024 * 64);
                let mut join_set = tokio::task::JoinSet::new();
                let (mut a_1_r, mut a_1_w) = tokio::io::split(a_1);
                let (mut b_1_r, mut b_1_w) = tokio::io::split(b_1);
                join_set.spawn({
                    let msg_list = msg_list.clone();
                    async move {
                        for _ in 0..epoch {
                            for msg in msg_list.as_ref() {
                                a_1_w.write_all(msg.as_bytes()).await.unwrap();
                            }
                        }
                    }
                });
                join_set.spawn({
                    let msg_list = msg_list.clone();
                    async move {
                        for _ in 0..epoch {
                            for msg in msg_list.as_ref() {
                                b_1_w.write_all(msg.as_bytes()).await.unwrap();
                            }
                        }
                    }
                });
                join_set.spawn({
                    let msg_list = msg_list.clone();
                    async move {
                        let mut buf = Vec::new();
                        for _ in 0..epoch {
                            for msg in msg_list.as_ref() {
                                buf.resize(msg.len(), 0);
                                a_1_r.read_exact(&mut buf).await.unwrap();
                                assert_eq!(buf, msg.as_bytes());
                            }
                        }
                    }
                });
                join_set.spawn({
                    let msg_list = msg_list.clone();
                    async move {
                        let mut buf = Vec::new();
                        for _ in 0..epoch {
                            for msg in msg_list.as_ref() {
                                buf.resize(msg.len(), 0);
                                b_1_r.read_exact(&mut buf).await.unwrap();
                                assert_eq!(buf, msg.as_bytes());
                            }
                        }
                    }
                });
                let (_a_to_b, _b_to_a) =
                    timed_copy_bidirectional(a_2, b_2, Limiter::new(f64::INFINITY))
                        .await
                        .unwrap();
                while let Some(res) = join_set.join_next().await {
                    res.unwrap();
                }
            });
        }
        while let Some(res) = join_set.join_next().await {
            res.unwrap();
        }
    }
}
