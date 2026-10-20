//go:build windows

package daemon

import (
	"os"
	"os/exec"
	"syscall"
)

// detachedProcess is the Win32 DETACHED_PROCESS creation flag: the child gets
// no console of its own.
const detachedProcess = 0x00000008

// detach starts the process without a console of its own, so closing the
// starting terminal never reaches it.
func detach(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{CreationFlags: syscall.CREATE_NEW_PROCESS_GROUP | detachedProcess}
}

// Alive reports whether pid refers to a running process. Processes belonging to
// other users may be invisible here and look dead.
func Alive(pid int) bool {
	if pid <= 0 {
		return false
	}
	// Opening a process only fails once no handle to it exists anywhere, so an
	// exited process still reachable through a foreign handle (runners and
	// antivirus hold such handles briefly) would look alive. An exit wait with a
	// zero timeout is the only reliable answer: WAIT_TIMEOUT means running.
	handle, err := syscall.OpenProcess(syscall.SYNCHRONIZE, false, uint32(pid))
	if err != nil {
		return false
	}
	defer syscall.CloseHandle(handle)
	event, err := syscall.WaitForSingleObject(handle, 0)
	return err == nil && event == syscall.WAIT_TIMEOUT
}

// terminate ends the process immediately; Windows offers no cross-process
// equivalent of SIGTERM, so in-flight requests are not drained.
func terminate(pid int) error {
	return kill(pid)
}

// kill ends the process immediately.
func kill(pid int) error {
	process, err := os.FindProcess(pid)
	if err != nil {
		return err
	}
	defer process.Release()
	return process.Kill()
}
