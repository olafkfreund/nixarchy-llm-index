package queue

// Enqueue appends a job to the work queue and wakes one idle worker.
func Enqueue(jobs chan<- string, job string) {
	jobs <- job
}
