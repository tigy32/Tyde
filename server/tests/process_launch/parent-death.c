#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <spawn.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
static void fail(const char *where) {perror(where);exit(120);}
static void check(int yes,const char *where) {if(!yes){errno=EPROTO;fail(where);}}
static void transfer(int s,char kind,pid_t pid,const int *fds,size_t n) {
 check(n<=3,"fixture handle bound");
 struct {char kind;pid_t pid;} body={kind,pid};char ancillary[CMSG_SPACE(sizeof(int)*3)]={0};
 struct iovec iov={&body,sizeof(body)};struct msghdr msg={.msg_iov=&iov,.msg_iovlen=1};
 if(n){msg.msg_control=ancillary;msg.msg_controllen=CMSG_SPACE(sizeof(int)*n);struct cmsghdr *c=CMSG_FIRSTHDR(&msg);c->cmsg_level=SOL_SOCKET;c->cmsg_type=SCM_RIGHTS;c->cmsg_len=CMSG_LEN(sizeof(int)*n);memcpy(CMSG_DATA(c),fds,sizeof(int)*n);}
 if(sendmsg(s,&msg,0)!=sizeof(body))fail("send fixture handles");
}
static char receive(int s,pid_t *pid,int *fds,size_t n) {
 struct {char kind;pid_t pid;} body;char ancillary[CMSG_SPACE(sizeof(int)*3)];struct iovec iov={&body,sizeof(body)};
 struct msghdr msg={.msg_iov=&iov,.msg_iovlen=1,.msg_control=ancillary,.msg_controllen=sizeof(ancillary)};
 if(recvmsg(s,&msg,0)!=sizeof(body))fail("receive fixture handles");
 *pid=body.pid;
 if(n){struct cmsghdr *c=CMSG_FIRSTHDR(&msg);check(c && c->cmsg_type==SCM_RIGHTS && c->cmsg_len==CMSG_LEN(sizeof(int)*n),"handle shape");memcpy(fds,CMSG_DATA(c),sizeof(int)*n);}
 return body.kind;
}
static int observer(void) {
 struct sock_filter code[]={BPF_STMT(BPF_LD|BPF_W|BPF_ABS,offsetof(struct seccomp_data,nr)),BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,SYS_prctl,0,1),BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_USER_NOTIF),BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ALLOW)};
 struct sock_fprog filter={.len=sizeof(code)/sizeof(code[0]),.filter=code};
 if(prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0))fail("fixture no-new-privileges");
 int fd=syscall(SYS_seccomp,SECCOMP_SET_MODE_FILTER,SECCOMP_FILTER_FLAG_NEW_LISTENER,&filter);if(fd<0)fail("fixture observer");return fd;
}
static void u32(unsigned char **p,uint32_t n){*(*p)++=n>>24;*(*p)++=n>>16;*(*p)++=n>>8;*(*p)++=n;}
static void field(unsigned char **p,const char *s){size_t n=strlen(s);u32(p,n);memcpy(*p,s,n);*p+=n;}
struct setup {int socket;int race;const char *binary;pid_t child;};
static void *launch(void *value) {
 struct setup *s=value;
 int control[2],in[2],out[2];if(socketpair(AF_UNIX,SOCK_STREAM|SOCK_CLOEXEC,0,control)||pipe2(in,O_CLOEXEC)||pipe2(out,O_CLOEXEC))fail("fixture pipes");
 int parent=syscall(SYS_pidfd_open,getpid(),0),exe=open(s->binary,O_RDONLY|O_CLOEXEC);if(parent<0 || exe<0)fail("fixture source handles");
 char creator_path[80];snprintf(creator_path,sizeof(creator_path),"/proc/self/task/%ld",syscall(SYS_gettid));
 int creator=open(creator_path,O_PATH|O_DIRECTORY|O_CLOEXEC);if(creator<0)fail("fixture creator anchor");
 int sources[]={control[1],parent,exe,creator,in[0],out[1]};int destinations[]={3,4,5,6,0,1};
 posix_spawn_file_actions_t actions;check(!posix_spawn_file_actions_init(&actions),"fixture actions");
 for(int i=0;i<6;i++){sources[i]=fcntl(sources[i],F_DUPFD_CLOEXEC,20);check(sources[i]>=20,"fixture high fd");check(!posix_spawn_file_actions_adddup2(&actions,sources[i],destinations[i]),"fixture duplicate");}
 int listener=-1;if(s->race){listener=observer();transfer(s->socket,'L',0,&listener,1);close(listener);}
 posix_spawnattr_t attr;check(!posix_spawnattr_init(&attr) && !posix_spawnattr_setpgroup(&attr,0) && !posix_spawnattr_setflags(&attr,POSIX_SPAWN_SETPGROUP),"fixture attributes");
 char *argv[]={"/proc/self/fd/5","--tyde-process-exec",NULL},*env[]={NULL};int error=posix_spawn(&s->child,argv[0],&actions,&attr,argv,env);if(error){errno=error;fail("fixture native creation");}
 posix_spawn_file_actions_destroy(&actions);posix_spawnattr_destroy(&attr);for(int i=0;i<6;i++)close(sources[i]);close(control[1]);close(in[0]);close(out[1]);close(parent);close(exe);close(creator);
 int pidfd=syscall(SYS_pidfd_open,s->child,0);if(pidfd<0)fail("fixture child pidfd");
 if(s->race){int fds[]={pidfd,control[0],in[1]};transfer(s->socket,'R',s->child,fds,3);char c;check(read(s->socket,&c,1)==1 && c=='E',"race parent exit instruction");if(s->race==3)return NULL;_exit(0);}
 unsigned char plan[128],*p=plan+8;*p++=0;*p++=0;*p++=0;*p++=0;field(&p,"/bin/cat");u32(&p,0);u32(&p,0);uint64_t length=p-plan-8;for(int i=0;i<8;i++)plan[i]=length>>(56-i*8);
 check(write(control[0],plan,p-plan)==p-plan,"fixture plan write");char status[16];ssize_t n=read(control[0],status,sizeof(status));check(n==4 && !memcmp(status,"TYRD",4),"fixture helper readiness");check(read(control[0],status,sizeof(status))==0,"fixture target exec");close(control[0]);
 check(write(in[1],"PING",4)==4 && read(out[0],status,sizeof(status))==4 && !memcmp(status,"PING",4),"fixture live target round trip");
 int fds[]={pidfd,in[1]};transfer(s->socket,'S',s->child,fds,2);char c;check(read(s->socket,&c,1)==1 && c=='E',"fixture parent/thread exit instruction");
 return NULL;
}
static int run(const char *binary,int mode) {
 int sockets[2];if(socketpair(AF_UNIX,SOCK_SEQPACKET|SOCK_CLOEXEC,0,sockets))fail("fixture controller");
 pid_t parent=fork();if(parent<0)fail("fixture parent");if(!parent){close(sockets[0]);struct setup s={sockets[1],mode>=2?mode:0,binary,0};if(mode==1 || mode==3){pthread_t thread;check(!pthread_create(&thread,NULL,launch,&s) && !pthread_join(thread,NULL),"fixture stable parent thread");if(mode==1){int status;check(waitpid(s.child,&status,0)==s.child && WIFSIGNALED(status) && WTERMSIG(status)==SIGKILL,"PDEATHSIG follows creating thread");}transfer(s.socket,'T',0,NULL,0);char c;check(read(s.socket,&c,1)==1 && c=='Q',"fixture process exit instruction");}else launch(&s);_exit(0);}
 close(sockets[1]);int fds[3],listener=-1;pid_t child;
 if(mode>=2){check(receive(sockets[0],&child,&listener,1)=='L',"fixture listener delivery");struct seccomp_notif request={0};check(ioctl(listener,SECCOMP_IOCTL_NOTIF_RECV,&request)==0 && request.data.args[0]==PR_SET_PDEATHSIG,"hold actual helper parent-death syscall");check(receive(sockets[0],&child,fds,3)=='R' && child==(pid_t)request.pid,"fixture race child ownership");check(write(sockets[0],"E",1)==1,"allow parent exit");int status;if(mode==2){check(waitpid(parent,&status,0)==parent && WIFEXITED(status) && WEXITSTATUS(status)==0,"fixture original parent exited");}else{pid_t ignored;check(receive(sockets[0],&ignored,NULL,0)=='T',"creator died before protection while parent lives");check(kill(parent,0)==0,"original parent still lives");}struct seccomp_notif_resp response={.id=request.id,.flags=SECCOMP_USER_NOTIF_FLAG_CONTINUE};check(!ioctl(listener,SECCOMP_IOCTL_NOTIF_SEND,&response),"continue helper after parent death");unsigned char error[9]={0};check(read(fds[1],error,4)==4,"fixture helper result marker");int rejected=0;
 if(!memcmp(error,"TYER",4)){check(read(fds[1],error+4,5)==5,"fixture helper native error");rejected=error[4]==1 && error[8]==ESRCH;}
 if(mode==3){struct pollfd exited={fds[0],POLLIN,0};check(poll(&exited,1,-1)==1,"rejected helper exits while creator process remains alive");check(write(sockets[0],"Q",1)==1,"finish race parent fixture");check(waitpid(parent,&status,0)==parent,"reap race parent");}
 check(waitpid(child,&status,0)==child,"reap race helper");
 if(!rejected){close(listener);for(int i=0;i<3;i++)close(fds[i]);close(sockets[0]);printf("case=creating_thread_dies_before_prctl FAIL original_creator_rejected=false target_not_admitted=true\n");fflush(stdout);return 0;}
 check(WIFEXITED(status) && WEXITSTATUS(status)==126,"race helper exits before provider exec");close(listener);for(int i=0;i<3;i++)close(fds[i]);}
 else{check(receive(sockets[0],&child,fds,2)=='S',"fixture target started");check(write(sockets[0],"E",1)==1,"allow creating thread exit");if(mode==1){pid_t ignored;check(receive(sockets[0],&ignored,NULL,0)=='T',"thread died while parent process stayed alive");check(kill(parent,0)==0,"original parent process remains alive");check(write(sockets[0],"Q",1)==1,"finish thread fixture");}int status;check(waitpid(parent,&status,0)==parent && WIFEXITED(status) && WEXITSTATUS(status)==0,"fixture parent exit");if(mode==0){struct pollfd ready={fds[0],POLLIN,0};check(poll(&ready,1,-1)==1,"provider dies with original parent");check(waitpid(child,&status,0)==child && WIFSIGNALED(status) && WTERMSIG(status)==SIGKILL,"parent death survives helper and provider exec");}close(fds[0]);close(fds[1]);}
 close(sockets[0]);printf("case=%s PASS\n",mode==0?"parent_process_death":mode==1?"creating_thread_death":mode==2?"parent_dies_before_prctl":"creating_thread_dies_before_prctl");fflush(stdout);return 1;
}
int main(int argc,char **argv){if(argc!=2)return 121;if(prctl(PR_SET_CHILD_SUBREAPER,1,0,0,0))fail("fixture-only subreaper");int ok=1;for(int mode=0;mode<4;mode++)ok=run(argv[1],mode)&&ok;return ok?0:1;}
