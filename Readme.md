# Http-cmp 
A low effort comparison of different http-clients, mostly comparing `axum` averhead, and uring efficiency 
when fitted into `hyper`. 

## Current results

`axum` carries functionally no overhead over a pure `hyper` implementation. 

My adaptation of `tokio-uring` into `hyper` has the same performance characteristics as 
both of the above, when running single-threaded (the `tokio-uring` runtime is only single threaded as far 
as I can tell). Seem to have slightly worse outliers, but I haven't measured percentiles yet.