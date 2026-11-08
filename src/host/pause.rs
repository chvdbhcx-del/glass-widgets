//! Idle pause, the fullscreen / lock guard, dimming and the self-report.
use super::*;

impl App {
    /// One timer, armed for the earliest widget schedule; nothing is armed (zero wake-ups) while no visible card animates.
    pub(super) unsafe fn update_tick_timer(&mut self) {
        match (self.live(), self.scene.next_due()) {
            (true, Some(d)) => {
                let ms = d.saturating_duration_since(Instant::now()).as_millis().clamp(1, 3_600_000) as u32;
                SetTimer(self.hwnd, T_TICK, ms, None);
            }
            _ => {
                let _ = KillTimer(self.hwnd, T_TICK);
            }
        }
    }

    /// Fullscreen guard (500 ms): hide while a fullscreen app is in front, pause the capture, re-stack on show.
    pub(super) unsafe fn guard(&mut self) {
        // the menu closes when its card is gone (removed by a layout reload, a profile switch...)
        if self.ui.menu.as_ref().is_some_and(|m| !m.closing) && self.scene.index_of_uid(self.menu_uid).is_none() {
            self.abort_menu();
        }
        // the prompt / the menu end when another window takes the foreground (a click elsewhere); a layer that never got the keyboard is asked again
        let prompt_up = self.ui.prompt.as_ref().is_some_and(|p| !p.closing);
        if prompt_up || self.ui.menu.as_ref().is_some_and(|m| !m.closing) {
            let fg = GetForegroundWindow();
            let mut cls = [0u16; 64];
            let n = GetClassNameW(fg, &mut cls);
            let popup = String::from_utf16_lossy(&cls[..n.max(0) as usize]) == "#32768";
            if fg == self.hwnd {
                self.prompt_was_fg = true;
            } else if self.prompt_was_fg {
                if !fg.0.is_null() && !popup && self.prompt_t0.elapsed() > Duration::from_millis(1000) {
                    if prompt_up {
                        self.end_prompt(None);
                    } else {
                        self.close_menu();
                    }
                }
            } else if self.prompt_t0.elapsed() < Duration::from_millis(2000) {
                force_foreground(self.hwnd);
                let _ = SetFocus(self.hwnd);
            }
        }
        // edit mode ends when another window takes the foreground (alt-tab, a click on another app)
        if self.editing && self.ui.edit && !self.in_menu && self.drag.is_none() {
            let fg = GetForegroundWindow();
            let mut cls = [0u16; 64];
            let n = GetClassNameW(fg, &mut cls);
            let popup = String::from_utf16_lossy(&cls[..n.max(0) as usize]) == "#32768";
            if fg == self.hwnd {
                self.was_fg = true;
            } else if self.was_fg && !fg.0.is_null() && !popup && self.t_edit.elapsed() > Duration::from_millis(1500) {
                self.leave_edit();
            }
        }
        // insurance against a lost WM_BACKDROP wake-up (the capture thread only posts when its mask was empty)
        let m = self.bd().take_dirty();
        if m != 0 && self.live() {
            self.redraw(m, false);
        }
        let front = app_in_front(self.hwnd);
        if front != self.app_front {
            self.app_front = front;
            self.update_dim();
        }
        let fs = self.user_hidden || foreground_is_fullscreen(self.hwnd, &self.mon);
        if fs && self.visible {
            self.abort_prompt();
            self.abort_menu();
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            self.visible = false;
            self.set_pause(autopause::HIDDEN, true);
        } else if !fs && !self.visible {
            self.show();
            self.set_pause(autopause::HIDDEN, false);
        } else if self.visible {
            self.guard_n += 1;
            self.scenes_tick();
            if self.guard_n % 8 == 0 && !self.ui.edit && self.ui.prompt.is_none() && self.ui.menu.is_none() {
                place_above_desktop(self.hwnd); // explorer restarts / shell re-stacks
            }
        }
        self.auto_pause();
    }

    /// The desk is shown and no reason to sleep is left.
    pub(super) fn live(&self) -> bool {
        self.visible && !self.pause.any()
    }

    /// Raise / clear one pause reason. The capture, the tick timer and the backdrop / tick driven redraws follow the union of the reasons.
    pub(super) unsafe fn set_pause(&mut self, bit: u8, on: bool) {
        if !self.pause.set(bit, on) {
            return;
        }
        let asleep = self.pause.any();
        if let Some(b) = &self.backdrop {
            b.set_paused(asleep);
            if !asleep {
                b.boost(); // full rate at once, the capture thread was parked
            }
        }
        self.update_tick_timer();
        if !asleep && self.visible {
            self.wake();
        }
    }

    /// The last reason is gone: the jobs that finished meanwhile are picked up and every card is repainted from the live backdrop now.
    pub(super) unsafe fn wake(&mut self) {
        for uid in std::mem::take(&mut self.missed_jobs) {
            self.on_job(uid);
        }
        let all = self.all_mask() | BIT_GALLERY | BIT_TOOLBAR;
        self.redraw(all, false);
    }

    /// The reasons window messages raise (session lock, monitors off, suspend), from autopause's atomic.
    pub(super) unsafe fn sync_pause(&mut self) {
        let raised = autopause::raised();
        for bit in [autopause::LOCKED, autopause::DISPLAY_OFF, autopause::SUSPEND] {
            self.set_pause(bit, !self.autopause_off && raised & bit != 0);
        }
        if self.secure_poll && !self.autopause_off {
            self.set_pause(autopause::SECURE, !input_desktop_open()); // the lock screen is a secure desktop: it ends with the unlock message
        }
    }

    /// Something is open the user works with: the desk is never put to sleep behind it.
    pub(super) fn interacting(&self) -> bool {
        self.editing || self.ui.edit || self.ui.settings || self.ui.gallery || self.ui.prompt.is_some() || self.ui.menu.is_some() || self.in_menu || self.drag.is_some() || !self.springs.is_empty() || self.ui.toast.is_some()
    }

    /// The cards in desktop px.
    pub(super) fn screen_cards(&self) -> Vec<RECT> {
        self.card_rects().iter().map(|c| RECT { left: c.left + self.mon.left, top: c.top + self.mon.top, right: c.right + self.mon.left, bottom: c.bottom + self.mon.top }).collect()
    }

    /// Guard step (500 ms) of the reasons that are polled: the messages again (a lost one must not strand a pause), the input desktop, and
    /// whether other windows hide every card (every 0.9 s while awake, every step while only that holds the desk asleep).
    pub(super) unsafe fn auto_pause(&mut self) {
        if self.autopause_off {
            return;
        }
        self.sync_pause();
        if self.pause.hard() || self.interacting() {
            self.set_pause(autopause::COVERED, false);
            return;
        }
        if !self.pause.has(autopause::COVERED) && self.last_occ.elapsed() < Duration::from_millis(900) {
            return;
        }
        self.last_occ = Instant::now();
        let cards = self.screen_cards();
        let wins = covering_windows(self.hwnd, &cards);
        self.set_pause(autopause::COVERED, autopause::all_hidden(&cards, &wins));
    }

    /// `widgetsDim`: monochrome cards while an app window is in front (auto) / always / never; off while editing.
    pub(super) unsafe fn update_dim(&mut self) {
        let want = !self.ui.edit
            && match self.settings.dim {
                layout::Dim::Always => true,
                layout::Dim::Never => false,
                layout::Dim::Auto => self.app_front,
            };
        if want != self.scene.dimmed() {
            self.scene.set_dim(want);
            if self.live() {
                let d = RECT { left: self.win.left, top: self.win.top, right: self.win.right, bottom: self.win.bottom };
                self.frame(d);
            }
        }
    }

    pub(super) unsafe fn report(&self) {
        let secs = self.t0.elapsed().as_secs_f64();
        let (acq, copied) = self.bd().stats();
        let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb);
        let cpu = (cpu_time_100ns() - self.cpu0) as f64 / 1e7;
        let steady = self.cpu_mark.map(|(t, c0)| (cpu_time_100ns() - c0) as f64 / 1e7 / t.elapsed().as_secs_f64() * 100.0);
        let frames = self.st.backdrop_batches + self.st.clock_frames;
        println!("backdrop={}", self.bd().name());
        println!("seconds={secs:.1}");
        println!("working_set_mb={:.1}", c.WorkingSetSize as f64 / 1048576.0);
        println!("private_mb={:.1}", c.PrivateUsage as f64 / 1048576.0);
        println!("cpu_percent_of_one_core={:.2}", cpu / secs * 100.0);
        if let Some(s) = steady {
            println!("cpu_percent_steady_state={s:.2}");
        }
        println!("frames_rendered={frames}");
        println!("  backdrop_batches={} card_redraws={} clock_frames={}", self.st.backdrop_batches, self.st.card_redraws, self.st.clock_frames);
        println!("duplication_frames={acq}");
        println!("duplication_frames_with_gpu_copy={copied}");
        println!("capture_rate_hz_at_exit={}", self.bd().rate_hz());
        println!("achieved_backdrop_fps={:.1}", self.st.backdrop_batches as f64 / secs);
        println!("achieved_clock_fps={:.1}", self.st.clock_frames as f64 / secs);
        println!("render_cpu_ms_avg={:.3}", self.st.render_ms / frames.max(1) as f64);
        println!("present_cpu_ms_avg={:.3}", self.st.present_ms / frames.max(1) as f64);
        println!("luminance_probes={}", self.st.lum_reads);
    }
}
