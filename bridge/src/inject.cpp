#include <Windows.h>
#include <cstdio>
BOOL RemoteLoadLibrary(HANDLE hProcess, LPCSTR lpLibPath)
{
  HANDLE hThread;
  HMODULE hKernel32; /* = GetModuleHandle ( "Kernel32" ); */
  LPVOID lpvLibPath; /* Pointer to the remote address */

  /* Allocate the space required for the string in our process */
  if ((lpvLibPath = VirtualAllocEx(hProcess, NULL, strlen(lpLibPath) + 1, MEM_COMMIT, PAGE_READWRITE)))
  {
    /* Remotely copy lpLibPath to the pointed address of lpvLibPath */
    WriteProcessMemory(hProcess, lpvLibPath, (LPVOID)lpLibPath, strlen(lpLibPath) + 1, NULL);

    /* Get Kernel32 from the memory.  Providing you didn't fuck your system, this should almost always work */
    GetModuleHandleExA(0, "Kernel32", &hKernel32);

    /* Execute LoadLibrary in our process remotely and see if the thread was executed successfuly */
    if ((hThread = CreateRemoteThread(hProcess, NULL, 0, (LPTHREAD_START_ROUTINE)(GetProcAddress(hKernel32, "LoadLibraryA")), lpvLibPath, 0, NULL)))
    {

      /* Free the remotely allocated string */
      VirtualFreeEx(hProcess, lpvLibPath, strlen(lpLibPath) + 1, MEM_RELEASE);

      /* Wait for the thread to finish and return.  When that's done, we'll have the DLL loaded in the process space */
      WaitForSingleObject(hThread, INFINITE);

      return TRUE;
    }
    return FALSE;
  }
  return FALSE;
}

extern "C" void inject(HWND win, const char *dll_path)
{
  printf("Find window: %X\n", (unsigned int)win);
  DWORD pid;
  GetWindowThreadProcessId(win, &pid);
  if (pid)
  {
    printf("Find PID: %X\n", pid);
    HANDLE process = OpenProcess(1, FALSE, pid);
    HANDLE currentProcess = GetCurrentProcess();
    if (process)
    {
      printf("Process opened (1): 0x%X\n", (unsigned int)process);
      HANDLE newProcess;
      if (DuplicateHandle(currentProcess, process, currentProcess, &newProcess, PROCESS_ALL_ACCESS, FALSE, DUPLICATE_CLOSE_SOURCE))
      {
        printf("Process opened (2): 0x%X\n", (unsigned int)newProcess);
        printf("Injecting...");
        RemoteLoadLibrary(newProcess, dll_path);
        printf("Injected.\n");
        WaitForSingleObject(newProcess, INFINITE);
        printf("Process ended.\n");
        CloseHandle(newProcess);
      }
      else
      {
        printf("Error: %X\n", GetLastError());
      }
    }
  }
}