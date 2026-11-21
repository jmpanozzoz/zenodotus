# Tasks

Tasks are the unit of work in Tokio. They are cheap to create and are scheduled cooperatively.

Spawn a task with the spawn function:

```rust
tokio::spawn(async {
    println!("hello");
});
```

Tasks run until they complete or are dropped by the runtime.
