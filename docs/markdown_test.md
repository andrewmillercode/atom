# Heading levels

Text with **bold**, *italic*, ***both***, ~~struck~~, `inline code`, a
[link](https://example.com), an autolink <https://www.rust-lang.org>, an image
![alt text](https://x/y.png), inline math $e^{i\pi} + 1 = 0$, paren math
\(\pi(x)\), and currency that must stay plain: $5 vs $10.

A paragraph long enough to wrap several times to check word-wrapping behavior
when the terminal is narrow, with a trailing hard break here:  
second line after the break.

Use a [reference link][ref] and a Setext heading below.

Setext heading
==============

> A blockquote paragraph
> continued on the next line.
>
> - quote with a nested
>   - bullet inside

Task list:

- [x] done
- [ ] to do

Ordered with nesting:

1. first
2. second
   - nested bullet
   - deeper one
3. third

Loose list (blank between items):

- loose item one

- loose item two

--------

```rust
fn main() {
    println!("fenced code");
}
```

```
plain fence with a blank line

inside it
```

| Metric | Value | Notes |
|:-------|------:|:-----:|
| left aligned | 1 | centered |
| pipes like `a\|b` | 22 | right |
| a long cell that wraps when the terminal is narrow | 333 | ok |

| CJK 表 | emoji |
|---|---|
| 宽度 | 🚀🎉 |

$$\sum_{k=1}^{n} k = \frac{n(n+1)}{2}$$

[ref]: https://example.com/docs
