package main

// runAndDrain keeps main alive until both listener lifecycles have completed.
// run and the optional peer lifecycle retain their own bounded shutdown budgets.
// Canceling even on startup failure also releases the peer listener.
func runAndDrain(run func() error, stop func(), peerDone <-chan struct{}) error {
	err := run()
	stop()
	if peerDone != nil {
		<-peerDone
	}
	return err
}
