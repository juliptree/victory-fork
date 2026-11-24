//! Twins of the console depth buffers that shaders write too (#970), with `XCL_DEPTH_UAV_TWINS=1`.
//!
//! The PC refuses a resource that is both a depth buffer and a UAV texture, so such a console
//! depth buffer loses ALLOW_UNORDERED_ACCESS (`device_x::pc_resource_desc`) and its UAVs were null
//! views – what the title's shaders wrote there was lost (a retail title clears or fills its D16
//! shadow maps that way before it renders depth into them). A twin is a texture of the same
//! typeless format and size with unordered access and no depth: the depth buffer's UAVs are views
//! of the twin (`real::dev_create_uav_x`), and at the title's barriers into the UNORDERED_ACCESS
//! layout the depth buffer is copied to the twin, at those out of it the twin back to the depth
//! buffer (`sync`). The twin is the depth buffer's private data, so it goes when the depth
//! buffer does.
//!
//! XCL applies none of the title's enhanced barriers (#146): on the PC such a depth buffer stays
//! in the state it was made in, which `note_created` keeps for the copies' transitions (not
//! known: DEPTH_WRITE, what a depth buffer placed for unordered access is made in). Depth and
//! stencil formats (two planes) have no twin: the PC has no UAV format for them.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// The private data key under which a depth buffer holds its twin.
const TWIN: GUID = GUID::from_u128(0x5c1b_7a4e_9f02_4d36_b0a8_3e6d_1f29_c970);

/// `XCL_DEPTH_UAV_TWINS=1`: depth buffers that shaders write get twins (off by default).
pub fn enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| std::env::var("XCL_DEPTH_UAV_TWINS").is_ok_and(|v| v.trim() == "1"))
}

/// The PC state each console depth buffer that is also a UAV was made in.
static MADE_IN: Mutex<Option<HashMap<usize, i32>>> = Mutex::new(None);

/// A console depth buffer that shaders write too was made (`device_x::create_placed_resource_x`)
/// in PC state `state`.
pub fn note_created(res: usize, state: u32) {
    if enabled() {
        MADE_IN.lock().unwrap().get_or_insert_with(HashMap::new).insert(res, state as i32);
    }
}

fn state_of(res: usize) -> D3D12_RESOURCE_STATES {
    let s = MADE_IN.lock().unwrap().as_ref().and_then(|m| m.get(&res).copied());
    D3D12_RESOURCE_STATES(s.unwrap_or(D3D12_RESOURCE_STATE_DEPTH_WRITE.0))
}

/// `res` is gone (`real::res_release`, its last reference): a resource made later at the same
/// address does not take its state.
pub fn forget(res: usize) {
    if let Some(m) = MADE_IN.lock().unwrap().as_mut() {
        m.remove(&res);
    }
}

/// The view of the twin for a UAV the title made without a description: the twin's format is
/// typeless, and a UAV of a typeless resource needs a typed one (R16_UNORM, R32_FLOAT).
pub fn twin_uav_desc(twin: &D3D12_RESOURCE_DESC) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let format = if twin.Format == DXGI_FORMAT_R16_TYPELESS { DXGI_FORMAT_R16_UNORM } else { DXGI_FORMAT_R32_FLOAT };
    let mut v = D3D12_UNORDERED_ACCESS_VIEW_DESC { Format: format, ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D, ..Default::default() };
    if twin.DepthOrArraySize > 1 {
        v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
        v.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV { MipSlice: 0, FirstArraySlice: 0, ArraySize: twin.DepthOrArraySize as u32, PlaneSlice: 0 };
    }
    v
}

/// The depth buffer's transitions around a copy, from the state it was made in to the copy's
/// and back – none when it is in the copy's state already (a transition to the same state is
/// refused).
fn depth_transitions(made: D3D12_RESOURCE_STATES, copy: D3D12_RESOURCE_STATES) -> Option<[(D3D12_RESOURCE_STATES, D3D12_RESOURCE_STATES); 2]> {
    (made != copy).then_some([(made, copy), (copy, made)])
}

/// The twin's description for a depth buffer's (None: no twin for it – a format of two planes,
/// several samples, or a resource that already has unordered access).
pub fn twin_desc(d: &D3D12_RESOURCE_DESC) -> Option<D3D12_RESOURCE_DESC> {
    let depth = d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 != 0 && d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0;
    if !depth || d.SampleDesc.Count != 1 || !matches!(d.Format, DXGI_FORMAT_R16_TYPELESS | DXGI_FORMAT_R32_TYPELESS) {
        return None;
    }
    let mut t = *d;
    t.Flags = D3D12_RESOURCE_FLAGS((d.Flags.0 & !(D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 | D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0)) | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0);
    t.Alignment = 0;
    Some(t)
}

/// The twin `res` has (None: none made).
pub unsafe fn twin_of(res: &ID3D12Resource) -> Option<ID3D12Resource> {
    let mut p: *mut c_void = std::ptr::null_mut();
    let mut size = std::mem::size_of::<*mut c_void>() as u32;
    res.GetPrivateData(&TWIN, &mut size, Some(&mut p as *mut _ as *mut c_void)).ok()?;
    // an interface kept with SetPrivateDataInterface comes back with a reference of its own
    (!p.is_null()).then(|| ID3D12Resource::from_raw(p))
}

/// The twin a UAV of `res` is made on – made now if `res` has none yet (None: twins are off, or
/// `res` gets none: `twin_desc`, or it could not be made).
pub unsafe fn twin_for_uav(res: &ID3D12Resource) -> Option<ID3D12Resource> {
    if !enabled() {
        return None;
    }
    make_twin(res)
}

/// `twin_for_uav` whether twins are on or not.
unsafe fn make_twin(res: &ID3D12Resource) -> Option<ID3D12Resource> {
    if let Some(t) = twin_of(res) {
        return Some(t);
    }
    let td = twin_desc(&res.GetDesc())?;
    let mut dev: Option<ID3D12Device> = None;
    res.GetDevice(&mut dev).ok()?;
    let props = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
    let mut twin: Option<ID3D12Resource> = None;
    static N: AtomicU64 = AtomicU64::new(0);
    if let Err(e) = dev?.CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, &td, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, None, &mut twin) {
        if N.fetch_add(1, Ordering::Relaxed) < 4 {
            crate::xlog_warn!("D3D12.X: no twin for a depth buffer that shaders write ({}x{}x{}, format {}): {e} – its UAVs are null views", td.Width, td.Height, td.DepthOrArraySize, td.Format.0);
        }
        return None;
    }
    let twin = twin?;
    res.SetPrivateDataInterface(&TWIN, &twin).ok()?;
    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 4 || n.is_power_of_two() {
        crate::xlog_info!("D3D12.X: a depth buffer that shaders write ({}x{}x{}, format {}) has a twin for its UAVs ({n} so far)", td.Width, td.Height, td.DepthOrArraySize, td.Format.0);
    }
    Some(twin)
}

fn transition(res: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // borrowed: the barrier holds no reference of its own
                pResource: unsafe { std::mem::transmute_copy(res) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// At a barrier of the title that moves depth buffer `res` into (`into_uav`) or out of the
/// UNORDERED_ACCESS layout: the depth buffer is copied to its twin, or the twin back to it, with
/// the transitions the copy needs around it (`barrier`, `copy`: the list's ResourceBarrier and
/// CopyResource(destination, source)). Nothing when `res` has no twin. Returns whether it copied.
pub unsafe fn sync(res: usize, into_uav: bool, barrier: &dyn Fn(&[D3D12_RESOURCE_BARRIER]), copy: &dyn Fn(&ID3D12Resource, &ID3D12Resource)) -> bool {
    let r = res as *mut c_void;
    let Some(depth) = ID3D12Resource::from_raw_borrowed(&r) else { return false };
    let Some(twin) = twin_of(depth) else { return false };
    let made = state_of(res);
    let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
    let (src, dst) = (D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
    let (depth_copy, twin_copy) = if into_uav { (src, dst) } else { (dst, src) };
    let around = depth_transitions(made, depth_copy);
    let mut before = vec![transition(&twin, uav, twin_copy)];
    let mut after = vec![transition(&twin, twin_copy, uav)];
    if let Some([(a, b), (c, d)]) = around {
        before.push(transition(depth, a, b));
        after.push(transition(depth, c, d));
    }
    barrier(&before);
    if into_uav {
        copy(&twin, depth);
    } else {
        copy(depth, &twin);
    }
    barrier(&after);
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 4 || n.is_power_of_two() {
        let d = depth.GetDesc();
        crate::xlog_info!("D3D12.X: a depth buffer and its twin ({}x{}x{}): {} ({n} copies so far)", d.Width, d.Height, d.DepthOrArraySize, if into_uav { "copied to the twin" } else { "the twin copied back" });
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depth_desc(format: DXGI_FORMAT, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 65536,
            Width: 768,
            Height: 768,
            DepthOrArraySize: 6,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: flags,
        }
    }

    /// A single-plane depth buffer without unordered access gets a twin of its format and size
    /// with unordered access and no depth; one of two planes, a UAV texture or MSAA does not.
    #[test]
    fn a_d16_or_d32_depth_buffer_gets_a_twin_with_unordered_access() {
        let ds = D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL;
        for f in [DXGI_FORMAT_R16_TYPELESS, DXGI_FORMAT_R32_TYPELESS] {
            let t = twin_desc(&depth_desc(f, ds | D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE)).unwrap();
            assert_eq!((t.Format, t.Width, t.Height, t.DepthOrArraySize, t.MipLevels, t.Alignment), (f, 768, 768, 6, 1, 0));
            assert_eq!(t.Flags, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
        }
        assert!(twin_desc(&depth_desc(DXGI_FORMAT_R24G8_TYPELESS, ds)).is_none(), "depth + stencil");
        assert!(twin_desc(&depth_desc(DXGI_FORMAT_R32G8X24_TYPELESS, ds)).is_none(), "depth + stencil");
        assert!(twin_desc(&depth_desc(DXGI_FORMAT_R16_TYPELESS, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS)).is_none(), "not a depth buffer");
        let mut msaa = depth_desc(DXGI_FORMAT_R32_TYPELESS, ds);
        msaa.SampleDesc.Count = 4;
        assert!(twin_desc(&msaa).is_none());
    }

    /// A depth buffer made in the copy's state gets no transition to that same state (D3D12
    /// refuses StateBefore == StateAfter); one made in another state goes there and back.
    #[test]
    fn a_depth_buffer_in_the_copys_state_is_not_moved_to_it() {
        let (dw, src, dst) = (D3D12_RESOURCE_STATE_DEPTH_WRITE, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
        assert_eq!(depth_transitions(dw, src), Some([(dw, src), (src, dw)]));
        assert_eq!(depth_transitions(src, src), None);
        assert_eq!(depth_transitions(dst, dst), None);
    }

    /// A UAV made without a description gets a typed view of the twin: R16_UNORM / R32_FLOAT,
    /// an array view over every slice of a cube or cascade.
    #[test]
    fn a_uav_without_a_description_is_a_typed_view_of_the_twin() {
        let t = twin_desc(&depth_desc(DXGI_FORMAT_R16_TYPELESS, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL)).unwrap();
        let v = twin_uav_desc(&t);
        assert_eq!((v.Format, v.ViewDimension), (DXGI_FORMAT_R16_UNORM, D3D12_UAV_DIMENSION_TEXTURE2DARRAY));
        assert_eq!(unsafe { v.Anonymous.Texture2DArray.ArraySize }, 6);
        let mut one = depth_desc(DXGI_FORMAT_R32_TYPELESS, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL);
        one.DepthOrArraySize = 1;
        let v = twin_uav_desc(&twin_desc(&one).unwrap());
        assert_eq!((v.Format, v.ViewDimension), (DXGI_FORMAT_R32_FLOAT, D3D12_UAV_DIMENSION_TEXTURE2D));
    }

    /// A depth buffer's state goes with it: a later one at the same address starts from
    /// DEPTH_WRITE, not from the state the first was made in.
    #[test]
    fn a_released_depth_buffers_state_is_forgotten() {
        let res = 0x7E57_9700usize;
        MADE_IN.lock().unwrap().get_or_insert_with(HashMap::new).insert(res, D3D12_RESOURCE_STATE_COPY_SOURCE.0);
        assert_eq!(state_of(res), D3D12_RESOURCE_STATE_COPY_SOURCE);
        forget(res);
        assert_eq!(state_of(res), D3D12_RESOURCE_STATE_DEPTH_WRITE);
    }

    #[test]
    fn gpu_a_uav_write_to_a_depth_buffer_goes_through_its_twin() {
        gpu_round_trip(DXGI_FORMAT_R16_TYPELESS, DXGI_FORMAT_D16_UNORM, DXGI_FORMAT_R16_UNORM, 2);
    }

    #[test]
    fn gpu_d32_depth_and_r32_float_uav_round_trip() {
        gpu_round_trip(DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R32_FLOAT, 4);
    }

    /// A depth clear reaches the twin, then a typed UAV write reaches the depth buffer.
    fn gpu_round_trip(texture_format: DXGI_FORMAT, dsv_format: DXGI_FORMAT, uav_format: DXGI_FORMAT, bytes_per_pixel: usize) {
        use crate::gpu::{adapter, fence, recomp::runner::skip_test};
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
        use windows::Win32::Graphics::Dxgi::*;

        unsafe {
            let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
            let selector = adapter::Selector::from_env_or("auto");
            let Some((chosen, chosen_desc)) = adapter::candidates(&factory, &selector).into_iter().next() else {
                skip_test(&adapter::no_match(&factory, &selector));
                return;
            };
            let mut dev: Option<ID3D12Device> = None;
            if let Err(e) = D3D12CreateDevice(&chosen, D3D_FEATURE_LEVEL_11_0, &mut dev) {
                skip_test(&format!("D3D12CreateDevice failed: {e}"));
                return;
            }
            let dev = dev.unwrap();
            let luid = dev.GetAdapterLuid();
            assert_eq!((luid.HighPart, luid.LowPart), (chosen_desc.AdapterLuid.HighPart, chosen_desc.AdapterLuid.LowPart));
            let actual: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid).unwrap();
            let info = actual.GetDesc1().unwrap();
            println!(
                "D3D12 adapter: {} (LUID {:08x}:{:08x}, PCI {:04x}:{:04x})",
                adapter::name(&info), luid.HighPart, luid.LowPart, info.VendorId, info.DeviceId
            );
            let (w, h) = (64u32, 32u32);
            let mut d = depth_desc(texture_format, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL);
            (d.Width, d.Height, d.DepthOrArraySize, d.Alignment) = (w as u64, h, 1, 0);
            let default = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
            let mut depth: Option<ID3D12Resource> = None;
            dev.CreateCommittedResource(&default, D3D12_HEAP_FLAG_NONE, &d, D3D12_RESOURCE_STATE_DEPTH_WRITE, None, &mut depth).unwrap();
            let depth = depth.unwrap();
            assert!(twin_of(&depth).is_none());
            let twin = make_twin(&depth).expect("a twin");
            assert_eq!(twin_of(&depth).map(|t| t.as_raw()), Some(twin.as_raw()), "kept with the depth buffer");
            assert_eq!(make_twin(&depth).map(|t| t.as_raw()), Some(twin.as_raw()), "made once");

            // the depth buffer's DSV, and the twin's UAV in a shader-visible and a CPU heap (for the clear)
            let heap = |ty, n, flags| -> ID3D12DescriptorHeap { dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: ty, NumDescriptors: n, Flags: flags, NodeMask: 0 }).unwrap() };
            let dsv_heap = heap(D3D12_DESCRIPTOR_HEAP_TYPE_DSV, 1, D3D12_DESCRIPTOR_HEAP_FLAG_NONE);
            let (gpu_heap, cpu_heap) = (heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE), heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1, D3D12_DESCRIPTOR_HEAP_FLAG_NONE));
            let dsv = dsv_heap.GetCPUDescriptorHandleForHeapStart();
            dev.CreateDepthStencilView(&depth, Some(&D3D12_DEPTH_STENCIL_VIEW_DESC { Format: dsv_format, ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D, ..Default::default() }), dsv);
            let uav = twin_uav_desc(&twin.GetDesc());
            assert_eq!(uav.Format, uav_format);
            dev.CreateUnorderedAccessView(&twin, None, Some(&uav), gpu_heap.GetCPUDescriptorHandleForHeapStart());
            dev.CreateUnorderedAccessView(&twin, None, Some(&uav), cpu_heap.GetCPUDescriptorHandleForHeapStart());

            // Both formats fit in a 256-byte aligned row at this width.
            let pitch = 256u32;
            assert!(w as usize * bytes_per_pixel <= pitch as usize);
            let rb_desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: (pitch * h) as u64,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let readback_props = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
            let q: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).unwrap();
            let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let mut fence_value = 0u64;
            // one list: `record`, then a copy of `src` (in `state`) to a readback buffer, run and read
            let run = |record: &dyn Fn(&ID3D12GraphicsCommandList), src: &ID3D12Resource, state: D3D12_RESOURCE_STATES, fence_value: &mut u64| -> Vec<u32> {
                let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).unwrap();
                record(&list);
                let mut rb: Option<ID3D12Resource> = None;
                dev.CreateCommittedResource(&readback_props, D3D12_HEAP_FLAG_NONE, &rb_desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut rb).unwrap();
                let rb = rb.unwrap();
                list.ResourceBarrier(&[transition(src, state, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: texture_format, Width: w, Height: h, Depth: 1, RowPitch: pitch },
                };
                let to = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&rb), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint } };
                let from = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(src), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
                list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
                list.ResourceBarrier(&[transition(src, D3D12_RESOURCE_STATE_COPY_SOURCE, state)]);
                list.Close().unwrap();
                q.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
                *fence_value += 1;
                q.Signal(&fence, *fence_value).unwrap();
                fence::wait(&fence, *fence_value, &dev, None, 10_000).unwrap();
                let mut p: *mut c_void = std::ptr::null_mut();
                rb.Map(0, None, Some(&mut p)).unwrap();
                let rows = std::slice::from_raw_parts(p as *const u8, (pitch * h) as usize);
                let px = rows.chunks_exact(pitch as usize)
                    .flat_map(|row| row[..w as usize * bytes_per_pixel].chunks_exact(bytes_per_pixel))
                    .map(|pixel| {
                        let mut bits = [0; 4];
                        bits[..bytes_per_pixel].copy_from_slice(pixel);
                        u32::from_le_bytes(bits)
                    })
                    .collect();
                rb.Unmap(0, None);
                px
            };
            let verify = |pixels: &[u32], expected: f32, stage: &str| {
                let min = *pixels.iter().min().unwrap();
                let max = *pixels.iter().max().unwrap();
                println!("{dsv_format:?}/{uav_format:?} {stage}: {} texels, raw range {min:#010x}..={max:#010x}", pixels.len());
                assert_eq!(pixels.len(), (w * h) as usize);
                if bytes_per_pixel == 4 {
                    assert!(pixels.iter().all(|&bits| f32::from_bits(bits) == expected), "{stage}: expected {expected}");
                } else {
                    let unorm = (expected * 65535.0).round() as u32;
                    assert!(pixels.iter().all(|&bits| bits.abs_diff(unorm) <= 1), "{stage}: expected approximately {unorm}");
                }
            };

            // the depth buffer cleared to 0.25, then the barrier into UNORDERED_ACCESS: the twin has it
            let into = |l: &ID3D12GraphicsCommandList| {
                l.ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, 0.25, 0, None);
                assert!(sync(depth.as_raw() as usize, true, &|b| l.ResourceBarrier(b), &|dst, src| l.CopyResource(dst, src)));
            };
            let twin_px = run(&into, &twin, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, &mut fence_value);
            verify(&twin_px, 0.25, "depth -> twin");

            // a write through the twin's UAV (0.75), then the barrier out of UNORDERED_ACCESS: the depth buffer has it
            let out = |l: &ID3D12GraphicsCommandList| {
                l.SetDescriptorHeaps(&[Some(gpu_heap.clone())]);
                l.ClearUnorderedAccessViewFloat(gpu_heap.GetGPUDescriptorHandleForHeapStart(), cpu_heap.GetCPUDescriptorHandleForHeapStart(), &twin, &[0.75, 0.0, 0.0, 0.0], &[]);
                assert!(sync(depth.as_raw() as usize, false, &|b| l.ResourceBarrier(b), &|dst, src| l.CopyResource(dst, src)));
            };
            let depth_px = run(&out, &depth, D3D12_RESOURCE_STATE_DEPTH_WRITE, &mut fence_value);
            verify(&depth_px, 0.75, "twin -> depth");
            assert_eq!(dev.GetDeviceRemovedReason(), Ok(()), "the device is still there");
        }
    }
}
