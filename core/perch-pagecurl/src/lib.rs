//! 仿真翻页（卷页）的**纯几何计算**。
//!
//! 对应项目目标里的分工："仿真翻页在原生自定义 View 做，**Rust 算路径**"。
//! 本 crate 只做数学，不碰渲染：输入"页面尺寸 + 拖到哪 + 卷曲半径"，输出
//! 折线、卷曲角、投影带宽度、正面可见多边形与阴影参数；Dart/Swift 负责把它们画出来。
//!
//! 这样做的好处：**手感调参就是在 Linux 上跑单测**，不需要设备、不需要 Mac。
//!
//! # 几何模型
//!
//! 采用经典的"折纸"模型，它同时给出了正确的折线与可信的卷曲：
//!
//! 1. **折线** = 被拖动的角 `C` 与手指位置 `F` 的**垂直平分线**。
//!    理由很直白：纸被折过去以后，角点必然落在手指处，而折痕就是到两点等距的轨迹。
//! 2. **圆柱**：折线是纸与圆柱的**相切**处，圆柱半径为 `r`，轴平行于折线，
//!    中心在折线向手指方向偏移 `r` 处。相切带来了光滑过渡——不会出现折痕感。
//! 3. **卷曲角**：角点到折线的纸上距离是 `h = |F - C| / 2`。这段纸全部绕到圆柱上，
//!    于是 `θ = h / r`（弧度），上限 `π`（绕半圈就是极限，再绕就要穿透了）。
//! 4. **剩余部分**：`h > θ·r` 时，绕过圆柱后还有一段纸沿切线直线延伸——这就是
//!    翻过去的那一片。
//!
//! # 投影（为什么只需要几个数字就能画出立体感）
//!
//! 圆柱面上的点 `P(θ) = A + r(-cosθ·u + sinθ·ẑ)`（`u` 是页面内指向手指的单位向量，
//! `ẑ` 垂直纸面）。投影到纸面后，沿 `u` 的偏移是 `r(1 - cosθ)`——**圆柱在投影上退化
//! 成一条带**：
//!
//! ```text
//! 折线                               纸边
//!  │←──── band_extent ────→│← straight·sinθ →│
//!  ╞═══════════════════════╪══════════════════╡
//!  ↑ 圆柱卷起部分（投影成带状）  ↑ 直线翻过去的部分
//! ```
//!
//! 立体感因此完全由**沿 `u` 方向的渐变**表达：曲率越大（θ 越大）扇面越暗。
//! 所以输出里给的是宽度与渐变参数，而不是网格——这也大幅降低了每帧的计算量。

use std::f32::consts::PI;

/// 二维点/向量（页面坐标系，`y` 向下，与 Flutter/SwiftUI 一致）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec2 {
    /// 横坐标。
    pub x: f32,
    /// 纵坐标。
    pub y: f32,
}

impl Vec2 {
    /// 构造。
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// 加法。
    #[must_use]
    pub const fn add(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y)
    }

    /// 减法。
    #[must_use]
    pub const fn sub(self, other: Self) -> Self {
        Self::new(self.x - other.x, self.y - other.y)
    }

    /// 缩放。
    #[must_use]
    pub const fn scale(self, factor: f32) -> Self {
        Self::new(self.x * factor, self.y * factor)
    }

    /// 点积。
    #[must_use]
    pub const fn dot(self, other: Self) -> f32 {
        self.x * other.x + self.y * other.y
    }

    /// 长度。
    #[must_use]
    pub fn length(self) -> f32 {
        self.dot(self).sqrt()
    }

    /// 单位化；零向量返回 `None`（避免产生 NaN）。
    #[must_use]
    pub fn normalized(self) -> Option<Self> {
        let length = self.length();
        if length > 1e-6 && length.is_finite() {
            Some(self.scale(1.0 / length))
        } else {
            None
        }
    }

    /// 逆时针旋转 90°（在 `y` 向下的坐标系里是"左侧"）。
    #[must_use]
    pub const fn perp(self) -> Self {
        Self::new(-self.y, self.x)
    }

    /// 是否全部有限（防止 NaN 传播到渲染层）。
    #[must_use]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// 页面尺寸（左上角为原点）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// 宽。
    pub width: f32,
    /// 高。
    pub height: f32,
}

impl Rect {
    /// 构造。
    #[must_use]
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    /// 对角线长度（用于把"拖了多远"归一化成进度）。
    #[must_use]
    pub fn diagonal(self) -> f32 {
        (self.width * self.width + self.height * self.height).sqrt()
    }

    /// 有效尺寸（宽高均为正且有限）。
    #[must_use]
    pub fn is_valid(self) -> bool {
        self.width > 0.0 && self.height > 0.0 && self.width.is_finite() && self.height.is_finite()
    }
}

/// 被拖动的角。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    /// 左上。
    TopLeft,
    /// 右上。
    TopRight,
    /// 左下。
    BottomLeft,
    /// 右下。
    BottomRight,
}

impl Corner {
    /// 该角在页面里的静止位置。
    #[must_use]
    pub fn rest_position(self, page: Rect) -> Vec2 {
        match self {
            Self::TopLeft => Vec2::new(0.0, 0.0),
            Self::TopRight => Vec2::new(page.width, 0.0),
            Self::BottomLeft => Vec2::new(0.0, page.height),
            Self::BottomRight => Vec2::new(page.width, page.height),
        }
    }
}

/// 卷页计算的输入。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurlInput {
    /// 页面尺寸。
    pub page: Rect,
    /// 正在被拖动的角。
    pub corner: Corner,
    /// 手指当前位置（页面坐标系；允许落在页面外）。
    pub drag: Vec2,
    /// 卷曲半径（页面单位）。越大越"圆润"，越小越"锐利"。
    pub radius: f32,
}

/// 阴影参数（一条沿卷曲方向的渐变）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowSpec {
    /// 渐变起点（折线上）。
    pub start: Vec2,
    /// 渐变终点（卷曲外侧）。
    pub end: Vec2,
    /// 起点不透明度（0~1）。
    pub start_opacity: f32,
    /// 终点不透明度（0~1）。
    pub end_opacity: f32,
}

/// 卷页几何的计算结果。
#[derive(Debug, Clone, PartialEq)]
pub struct CurlGeometry {
    /// 折线与页面矩形的两个交点（已裁剪到页面内）。
    pub fold_start: Vec2,
    /// 折线与页面矩形的另一个交点。
    pub fold_end: Vec2,
    /// 折线中点，也是纸面与圆柱相切的位置。
    pub midpoint: Vec2,
    /// 折线中点 → 手指的单位方向。
    pub direction: Vec2,
    /// 圆柱半径。
    pub radius: f32,
    /// 卷曲角（弧度，0~π）：纸上从折线到被拖动的角，一共绕了多少弧度。
    pub curl_angle: f32,
    /// 卷曲部分在**投影**上的可见宽度。
    ///
    /// 等于 `r·sin(min(φ, π/2))`：φ 小的时候纸边自己就是边界；
    /// φ 超过 π/2 后纸已绕到圆柱背面（被自身挡住），边界就变成圆柱的轮廓线 `r`。
    pub band_extent: f32,
    /// 拖拽进度 0~1（按"拖动距离 / 对角线"归一化，供动画与判定使用）。
    pub progress: f32,
    /// 正面（还没被卷起）的可见多边形，逆时针顺序。
    pub front_outline: Vec<Vec2>,
    /// 投影在纸面上的阴影渐变。
    pub shadow: ShadowSpec,
}

/// 计算卷页几何。输入退化（没拖动、尺寸非法、折线不与页面相交）时返回 `None`。
#[must_use]
pub fn curl(input: &CurlInput) -> Option<CurlGeometry> {
    // 显式判有限性而不是用 `!(radius > 0.0)`：后者虽然能顺带挡住 NaN，
    // 但把"非正"和"不是数"两件事混在一句取反比较里，可读性差
    if !input.page.is_valid() || !input.radius.is_finite() || input.radius <= 0.0 {
        return None;
    }
    if !input.drag.is_finite() {
        return None;
    }

    let corner = input.corner.rest_position(input.page);
    let to_finger = input.drag.sub(corner);
    // 折线到角的距离：角与手指之间的纸长一半
    let half = to_finger.length() / 2.0;
    if !half.is_finite() || half <= 1e-3 {
        // 没拖动（或拖得可忽略）→ 没有卷曲
        return None;
    }

    let direction = to_finger.normalized()?;
    let midpoint = corner.add(input.drag).scale(0.5);
    let fold_direction = direction.perp();

    let radius = input.radius;
    // 纸上从折线到角的长度是 half，全部绕到圆柱上 → 卷角 φ = half / r。
    //
    // 圆柱中心在纸面**正上方**（M + r·ẑ）而不是沿纸面偏移：因为纸是平着进入
    // 折线的，切线必须落在纸面内，否则折线处会出现折痕感（不满足切线连续）。
    // 由此得到圆柱面上的点 P(φ) = M + r·sinφ·u + r(1-cosφ)·ẑ，
    // 投影后沿 u 的偏移是 r·sinφ —— 注意**不是** r(1-cosφ)。
    let curl_angle = (half / radius).min(PI);
    // φ 超过 π/2 后纸绕到圆柱背面，被自身遮挡，投影边界变成圆柱轮廓（r）
    let visible_angle = curl_angle.min(PI / 2.0);
    let band_extent = radius * visible_angle.sin();

    let (fold_start, fold_end) = clip_line_to_rect(midpoint, fold_direction, input.page)?;
    let front_outline = clip_rect_by_half_plane(input.page, midpoint, direction)?;

    let progress = (to_finger.length() / input.page.diagonal()).clamp(0.0, 1.0);

    // 阴影：从折线向手指方向淡出（卷得越多，压在下层页面上的影子越重）
    let shadow_extent = band_extent.max(1.0) + radius * 0.5;
    let shadow = ShadowSpec {
        start: midpoint,
        end: midpoint.add(direction.scale(shadow_extent)),
        start_opacity: (0.10 + 0.35 * (curl_angle / PI)).min(0.6),
        end_opacity: 0.0,
    };

    Some(CurlGeometry {
        fold_start,
        fold_end,
        midpoint,
        direction,
        radius,
        curl_angle,
        band_extent,
        progress,
        front_outline,
        shadow,
    })
}

/// 松手后是否应当**完成**这次翻页（否则回弹）。
///
/// 判定糅合两个信号：拖得够远，或者甩得够快。两者都由调用方给阈值——
/// 手感参数不该写死在几何模块里，而应当在真机上调（见 `docs/08-仿真翻页方案.md`）。
#[must_use]
pub fn should_complete(
    travel: f32,
    page: Rect,
    velocity: f32,
    travel_ratio: f32,
    velocity_threshold: f32,
) -> bool {
    if !page.is_valid() {
        return false;
    }
    let travelled_enough = travel / page.diagonal() >= travel_ratio;
    let flicked_enough = velocity.abs() >= velocity_threshold;
    travelled_enough || flicked_enough
}

/// 把过 `point`、方向 `direction` 的**直线**裁剪到页面矩形，返回线段端点。
///
/// 用 Liang–Barsky 的思路按两条轴分别求参数区间再取交集。
fn clip_line_to_rect(point: Vec2, direction: Vec2, page: Rect) -> Option<(Vec2, Vec2)> {
    let mut t_min = f32::NEG_INFINITY;
    let mut t_max = f32::INFINITY;

    for (origin, delta, lower, upper) in [
        (point.x, direction.x, 0.0, page.width),
        (point.y, direction.y, 0.0, page.height),
    ] {
        if delta.abs() < 1e-6 {
            // 与该边界平行：落在带外则整条线都在矩形外
            if origin < lower || origin > upper {
                return None;
            }
            continue;
        }
        let first = (lower - origin) / delta;
        let second = (upper - origin) / delta;
        let (near, far) = if first <= second {
            (first, second)
        } else {
            (second, first)
        };
        t_min = t_min.max(near);
        t_max = t_max.min(far);
    }

    if !t_min.is_finite() || !t_max.is_finite() || t_min > t_max {
        return None;
    }

    let start = point.add(direction.scale(t_min));
    let end = point.add(direction.scale(t_max));
    if !start.is_finite() || !end.is_finite() {
        return None;
    }
    Some((start, end))
}

/// 把页面矩形按半平面 `(P - m)·n >= 0` 裁剪成多边形（Sutherland–Hodgman）。
///
/// 保留的是**没有被卷起**的那一侧，也就是含手指的那一侧。
/// 这一点很容易搞反：被卷起的是"靠近被抓的角"那一半（角在 `-h`，手指在 `+h`），
/// 剩下不动的是手指那一侧。
fn clip_rect_by_half_plane(page: Rect, m: Vec2, n: Vec2) -> Option<Vec<Vec2>> {
    let rect = [
        Vec2::new(0.0, 0.0),
        Vec2::new(page.width, 0.0),
        Vec2::new(page.width, page.height),
        Vec2::new(0.0, page.height),
    ];

    let mut output: Vec<Vec2> = Vec::with_capacity(5);
    for index in 0..rect.len() {
        let current = rect[index];
        let next = rect[(index + 1) % rect.len()];
        let current_distance = current.sub(m).dot(n);
        let next_distance = next.sub(m).dot(n);
        let current_inside = current_distance >= 0.0;
        let next_inside = next_distance >= 0.0;

        if current_inside {
            output.push(current);
        }
        // 跨越半平面时补上交点
        if current_inside != next_inside {
            let denominator = current_distance - next_distance;
            if denominator.abs() > 1e-9 {
                let ratio = current_distance / denominator;
                output.push(current.add(next.sub(current).scale(ratio)));
            }
        }
    }

    if output.len() < 3 { None } else { Some(output) }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::f32::consts::PI;

    fn page() -> Rect {
        Rect::new(400.0, 700.0)
    }

    fn near(left: f32, right: f32) -> bool {
        (left - right).abs() < 1e-2
    }

    #[test]
    fn no_drag_means_no_curl() {
        let input = CurlInput {
            page: page(),
            corner: Corner::BottomRight,
            drag: Corner::BottomRight.rest_position(page()),
            radius: 20.0,
        };
        assert!(curl(&input).is_none());
    }

    #[test]
    fn degenerate_inputs_are_rejected() {
        let base = CurlInput {
            page: page(),
            corner: Corner::BottomRight,
            drag: Vec2::new(200.0, 350.0),
            radius: 20.0,
        };

        // 半径为 0
        let zero_radius = CurlInput {
            radius: 0.0,
            ..base
        };
        assert!(curl(&zero_radius).is_none());

        // 页面尺寸非法
        let bad_page = CurlInput {
            page: Rect::new(0.0, 700.0),
            ..base
        };
        assert!(curl(&bad_page).is_none());

        // 手指位置含 NaN
        let nan_drag = CurlInput {
            drag: Vec2::new(f32::NAN, 0.0),
            ..base
        };
        assert!(curl(&nan_drag).is_none());
    }

    #[test]
    fn fold_line_is_the_perpendicular_bisector() {
        let corner = Corner::BottomRight.rest_position(page());
        let drag = Vec2::new(150.0, 300.0);
        let geometry = curl(&CurlInput {
            page: page(),
            corner: Corner::BottomRight,
            drag,
            radius: 24.0,
        })
        .unwrap();

        // 折线中点 = 角与手指的中点
        let expected_midpoint = corner.add(drag).scale(0.5);
        assert!(near(geometry.midpoint.x, expected_midpoint.x));
        assert!(near(geometry.midpoint.y, expected_midpoint.y));

        // 折线方向 ⟂ (手指 - 角)
        let to_finger = drag.sub(corner);
        let fold_direction = geometry.fold_end.sub(geometry.fold_start);
        assert!(
            near(fold_direction.dot(to_finger), 0.0),
            "折线与角→手指应垂直"
        );

        // 折线到角与到手指的距离相等（垂直平分线的定义）
        let to_corner = geometry.midpoint.sub(corner).length();
        let to_finger = geometry.midpoint.sub(drag).length();
        assert!(near(to_corner, to_finger));
    }

    #[test]
    fn fold_line_is_clipped_into_the_page() {
        let geometry = curl(&CurlInput {
            page: page(),
            corner: Corner::BottomRight,
            drag: Vec2::new(120.0, 280.0),
            radius: 30.0,
        })
        .unwrap();

        for point in [geometry.fold_start, geometry.fold_end] {
            assert!(
                point.x >= -0.01 && point.x <= page().width + 0.01,
                "{point:?}"
            );
            assert!(
                point.y >= -0.01 && point.y <= page().height + 0.01,
                "{point:?}"
            );
        }
    }

    #[test]
    fn curl_angle_follows_drag_and_clamps_at_pi() {
        let corner = Corner::BottomRight;
        // 半径取大一点，让循环里的三次拖拽都不触发 π 钳制，才能测出单调性
        let radius = 200.0;

        let mut previous = 0.0;
        for distance in [60.0, 200.0, 400.0] {
            let drag = Vec2::new(page().width, page().height - distance);
            let geometry = curl(&CurlInput {
                page: page(),
                corner,
                drag,
                radius,
            })
            .unwrap();
            assert!(
                geometry.curl_angle > previous,
                "拖得越远卷得越多：{} vs {}",
                geometry.curl_angle,
                previous
            );
            previous = geometry.curl_angle;
        }

        // 半径很小 + 拖得很远 → 卷到极限 π
        let geometry = curl(&CurlInput {
            page: page(),
            corner,
            drag: Vec2::new(page().width, 0.0),
            radius: 1.0,
        })
        .unwrap();
        assert!(near(geometry.curl_angle, PI), "应钳制在 π");
    }

    #[test]
    fn band_extent_matches_the_projection_formula() {
        let geometry = curl(&CurlInput {
            page: page(),
            corner: Corner::BottomRight,
            drag: Vec2::new(200.0, 400.0),
            radius: 26.0,
        })
        .unwrap();

        // 带宽 = r·sin(min(φ, π/2))
        let visible = geometry.curl_angle.min(PI / 2.0);
        let expected = geometry.radius * visible.sin();
        assert!(near(geometry.band_extent, expected));
        // 上限就是圆柱半径
        assert!(geometry.band_extent <= geometry.radius + 1e-3);
    }

    #[test]
    fn band_is_limited_by_the_cylinder_silhouette() {
        let corner = Corner::BottomRight;

        // 半径很大：纸只卷了一点点，带宽 ≈ r·sinφ，远小于 r
        let barely = curl(&CurlInput {
            page: page(),
            corner,
            drag: Vec2::new(page().width - 20.0, page().height - 20.0),
            radius: 500.0,
        })
        .unwrap();
        assert!(barely.curl_angle < PI / 2.0);
        assert!(near(
            barely.band_extent,
            barely.radius * barely.curl_angle.sin()
        ));
        assert!(barely.band_extent < barely.radius);

        // 半径很小 + 拖得远：φ 达到 π，纸绕到圆柱背面，
        // 投影边界变成圆柱轮廓 = 半径（封顶，而不是继续增长）
        let wrapped = curl(&CurlInput {
            page: page(),
            corner,
            drag: Vec2::new(100.0, 200.0),
            radius: 5.0,
        })
        .unwrap();
        assert!(near(wrapped.curl_angle, PI));
        assert!(near(wrapped.band_extent, wrapped.radius), "应封顶在半径");
    }

    #[test]
    fn all_four_corners_produce_geometry() {
        let drag = Vec2::new(200.0, 350.0);
        for corner in [
            Corner::TopLeft,
            Corner::TopRight,
            Corner::BottomLeft,
            Corner::BottomRight,
        ] {
            let geometry = curl(&CurlInput {
                page: page(),
                corner,
                drag,
                radius: 22.0,
            })
            .unwrap();

            let rest = corner.rest_position(page());
            let expected_midpoint = rest.add(drag).scale(0.5);
            assert!(near(geometry.midpoint.x, expected_midpoint.x));
            assert!(near(geometry.midpoint.y, expected_midpoint.y));
            assert!(geometry.fold_start.is_finite());
            assert!(geometry.fold_end.is_finite());
        }
    }

    #[test]
    fn front_outline_is_inside_the_page_and_on_the_flat_side() {
        let geometry = curl(&CurlInput {
            page: page(),
            corner: Corner::BottomRight,
            drag: Vec2::new(180.0, 320.0),
            radius: 28.0,
        })
        .unwrap();

        assert!(geometry.front_outline.len() >= 3);
        for point in &geometry.front_outline {
            assert!(point.is_finite());
            // 落在页面内（留一点浮点余量）
            assert!(
                point.x >= -0.05 && point.x <= page().width + 0.05,
                "{point:?}"
            );
            assert!(
                point.y >= -0.05 && point.y <= page().height + 0.05,
                "{point:?}"
            );
            // 全部在"未被卷起"的一侧（手指那一侧）：到折线的有向距离 >= 0
            let signed = point.sub(geometry.midpoint).dot(geometry.direction);
            assert!(signed >= -0.05, "正面多边形不该越过折线：{signed}");
        }
    }

    #[test]
    fn dragging_toward_the_opposite_corner_shrinks_the_flat_area() {
        let corner = Corner::BottomRight;
        let area = |drag: Vec2| {
            let geometry = curl(&CurlInput {
                page: page(),
                corner,
                drag,
                radius: 20.0,
            })
            .unwrap();
            // 用鞋带公式算多边形面积
            let points = &geometry.front_outline;
            let mut sum = 0.0;
            for index in 0..points.len() {
                let current = points[index];
                let next = points[(index + 1) % points.len()];
                sum += current.x * next.y - next.x * current.y;
            }
            (sum / 2.0).abs()
        };

        let slight = area(Vec2::new(page().width - 30.0, page().height - 30.0));
        let heavy = area(Vec2::new(100.0, 200.0));
        assert!(heavy < slight, "拖得越远，剩下的正面越小");
    }

    #[test]
    fn progress_is_bounded_and_monotonic() {
        let corner = Corner::BottomRight;
        let mut previous = -1.0;
        // 拖拽点保持在页面内（拖到页面外属于调用方该约束的情形，见下一个测试）
        for distance in [20.0, 150.0, 300.0, 380.0] {
            let geometry = curl(&CurlInput {
                page: page(),
                corner,
                drag: Vec2::new(page().width - distance, page().height - distance),
                radius: 20.0,
            })
            .unwrap();
            assert!((0.0..=1.0).contains(&geometry.progress));
            assert!(geometry.progress > previous);
            previous = geometry.progress;
        }
    }

    #[test]
    fn fold_line_missing_the_page_returns_none() {
        // 手指被拖到离页面很远的地方时，垂直平分线可能整个落在页面之外——
        // 此时页面内没有任何一条折痕，几何无从谈起。
        // 这是调用方需要约束拖拽范围的理由（手势处理时把手指钳制在合理范围内）。
        let far_away = curl(&CurlInput {
            page: page(),
            corner: Corner::BottomRight,
            drag: Vec2::new(-5000.0, -5000.0),
            radius: 20.0,
        });
        assert!(far_away.is_none());
    }

    #[test]
    fn direction_is_unit_length_and_shadow_is_sane() {
        let geometry = curl(&CurlInput {
            page: page(),
            corner: Corner::BottomRight,
            drag: Vec2::new(240.0, 420.0),
            radius: 25.0,
        })
        .unwrap();

        assert!(near(geometry.direction.length(), 1.0));
        assert!((0.0..=1.0).contains(&geometry.shadow.start_opacity));
        assert!((0.0..=1.0).contains(&geometry.shadow.end_opacity));
        assert!(geometry.shadow.start.is_finite());
        assert!(geometry.shadow.end.is_finite());
        // 阴影从折线出发，朝手指方向淡出
        let cast = geometry.shadow.end.sub(geometry.shadow.start);
        assert!(cast.dot(geometry.direction) > 0.0);
    }

    #[test]
    fn should_complete_on_travel_or_flick() {
        let page = page();
        // 拖得够远
        assert!(should_complete(400.0, page, 0.0, 0.25, 800.0));
        // 甩得够快
        assert!(should_complete(10.0, page, 1200.0, 0.25, 800.0));
        // 又短又慢 → 回弹
        assert!(!should_complete(20.0, page, 100.0, 0.25, 800.0));
        // 页面尺寸非法时一律回弹
        assert!(!should_complete(
            400.0,
            Rect::new(0.0, 0.0),
            0.0,
            0.25,
            800.0
        ));
        // 反向甩动也算"够快"（由调用方决定方向含义）
        assert!(should_complete(10.0, page, -1200.0, 0.25, 800.0));
    }

    #[test]
    fn vec2_helpers_behave() {
        let a = Vec2::new(3.0, 4.0);
        assert!(near(a.length(), 5.0));
        let unit = a.normalized().unwrap();
        assert!(near(unit.length(), 1.0));
        assert!(near(unit.x, 0.6));

        // 零向量不能单位化（避免 NaN）
        assert!(Vec2::new(0.0, 0.0).normalized().is_none());
        assert!(Vec2::new(f32::NAN, 0.0).normalized().is_none());

        assert!(near(Vec2::new(1.0, 0.0).perp().x, 0.0));
        assert!(near(Vec2::new(1.0, 0.0).perp().y, 1.0));
    }
}
